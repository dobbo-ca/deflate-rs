#![no_main]
use deflate::{Error, Limits};
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::AtomicBool;

#[path = "../common.rs"]
mod common;

// data = [cancel: u8][prefix: u8][max_output / 16: u16 LE][max_alloc / 16: u16 LE][stream]: limits
// up to 1 MiB, a prefix of `prefix` bytes already in `out` (so the fast loop runs with history
// below it), and bit 0 of `cancel` sets the cancel flag before the call.
// Raw DEFLATE: any input is Ok within the output limit or an error, never a panic. The fast loop
// equals the careful path alone: the same output, the same error, the same output before it.
// A set flag is Cancelled with nothing appended. Where both succeed, the output equals miniz_oxide's
// (an independent decoder: the two paths above share the tables and the bit reader). A stream B
// decoded after A on this thread (the spare Huffman tables are reused) gives what a fresh thread
// gives.
fuzz_target!(|data: &[u8]| {
    common::check_tier();
    let Some((&[cancel, pre, a, b, c, d], src)) = data.split_first_chunk::<6>() else {
        return;
    };
    let limits = Limits {
        max_output: usize::from(u16::from_le_bytes([a, b])) * 16,
        max_alloc: usize::from(u16::from_le_bytes([c, d])) * 16,
    };
    let stop = AtomicBool::new(cancel & 1 != 0);
    let pre: Vec<u8> = (0..pre).map(|i| i.wrapping_mul(7)).collect();
    let mut out = pre.clone();
    let r = deflate::decode_into(src, &mut out, &limits, &stop);
    assert!(out.starts_with(&pre));
    // `max_alloc` is charged before every growth: the buffer never ends up bigger than it allows
    // (plus what the caller already had), whatever the stream says.
    assert!(
        out.capacity() <= pre.capacity().max(pre.len() + limits.max_alloc),
        "capacity {} past max_alloc {}",
        out.capacity(),
        limits.max_alloc
    );
    assert!(out.len() - pre.len() <= limits.max_output);
    if cancel & 1 != 0 {
        assert_eq!(r, Err(Error::Cancelled));
        assert!(out == pre);
        return;
    }
    if let Err(e) = &r {
        assert!(*e != Error::Cancelled);
    }
    let mut slow = pre.clone();
    let rs = deflate::decode_careful_into(src, usize::MAX, &mut slow, &limits, &stop);
    assert_eq!(r, rs);
    assert!(
        out == slow,
        "output differs: {} and {} bytes",
        out.len(),
        slow.len()
    );
    if r.is_ok() {
        let cap = limits.max_output.saturating_add(1);
        if let Ok(m) = miniz_oxide::inflate::decompress_to_vec_with_limit(src, cap) {
            assert!(out[pre.len()..] == m[..], "differs from miniz_oxide");
        }
    }
    // Reuse of the thread's spare tables: decode A, then B, equals B alone on a fresh thread.
    if let Some((&k, rest)) = src.split_first() {
        let (a, b) = rest.split_at(usize::from(k) % (rest.len() + 1));
        let run = |s: &[u8]| {
            let mut o = Vec::new();
            let r = deflate::decode_into(s, &mut o, &limits, &stop);
            (r, o)
        };
        run(a);
        let again = run(b);
        let fresh = std::thread::scope(|sc| sc.spawn(|| run(b)).join().ok());
        assert!(Some(again) == fresh, "reused tables change the result");
    }
});
