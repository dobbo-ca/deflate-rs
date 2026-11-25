#![no_main]
use deflate::{Error, Limits};
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::AtomicBool;

#[path = "../common.rs"]
mod common;

// data = [cancel: u8][prefix: u8][max_output / 16: u16 LE][max_alloc / 16: u16 LE][stream]: as the
// `decode` target. zlib: Ok within the output limit or an error, never a panic; with the flag set
// it is an error that appends nothing.
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
    let r = deflate::zlib::decode_into(src, &mut out, &limits, &stop);
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
        assert!(r.is_err());
        assert!(out == pre);
    } else if let Err(e) = &r {
        assert!(*e != Error::Cancelled);
    }
    // Where both succeed, the output equals miniz_oxide's.
    if r.is_ok() {
        let cap = limits.max_output.saturating_add(1);
        if let Ok(m) = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(src, cap) {
            assert!(out[pre.len()..] == m[..], "differs from miniz_oxide");
        }
    }
});
