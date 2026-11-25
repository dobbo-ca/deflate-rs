#![no_main]
use deflate::{Limits, zlib};
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::AtomicBool;

#[path = "../common.rs"]
mod common;

// data = [raw or zlib: u8][n: u32 LE][prefix: u8][max_output / 16: u16 LE][max_alloc / 16: u16 LE]
// [stream]. A limited decode that succeeds is a prefix of at most n bytes, and of the whole
// decode when that succeeds. Bit 2 of the first byte sets the cancel flag for the raw
// comparison below, which has `prefix` bytes in `out`.
fuzz_target!(|data: &[u8]| {
    common::check_tier();
    let Some((&[kind, a, b, c, d, pre, e, f, g, h], src)) = data.split_first_chunk::<10>() else {
        return;
    };
    let limits = Limits {
        max_output: usize::from(u16::from_le_bytes([e, f])) * 16,
        max_alloc: usize::from(u16::from_le_bytes([g, h])) * 16,
    };
    let limits = &limits;
    let mut n = u32::from_le_bytes([a, b, c, d]) as usize % (1 << 20);
    if kind & 2 != 0 {
        n &= !(32768 - 1); // stdlib's window flush
    }
    let no = AtomicBool::new(false);
    let (full, part) = if kind & 1 == 0 {
        (
            deflate::decode(src, limits, &no),
            deflate::decode_limit(src, n, limits, &no),
        )
    } else {
        (
            zlib::decode(src, limits, &no),
            zlib::decode_limit(src, n, limits, &no),
        )
    };
    if kind & 1 == 0 {
        // The fast loop equals the careful path alone, also at a limit.
        // With a prefix in `out`, and the flag set when bit 2 says so (n == 0 is a no-op).
        let stop = AtomicBool::new(kind & 4 != 0);
        let pre: Vec<u8> = (0..pre).map(|i| i.wrapping_mul(7)).collect();
        let (mut fast, mut slow) = (pre.clone(), pre.clone());
        let rf = deflate::decode_limit_into(src, n, &mut fast, limits, &stop);
        let rs = deflate::decode_careful_into(src, n, &mut slow, limits, &stop);
        assert_eq!(rf, rs);
        assert!(fast == slow);
        assert!(fast.starts_with(&pre));
        assert!(
            fast.capacity() <= pre.capacity().max(pre.len() + limits.max_alloc),
            "capacity {} past max_alloc {}",
            fast.capacity(),
            limits.max_alloc
        );
        if kind & 4 != 0 && n > 0 {
            assert_eq!(rf, Err(deflate::Error::Cancelled));
            assert!(fast == pre);
        }
    }
    if let Ok(part) = &part {
        assert!(part.len() <= n);
    }
    if let (Ok(full), Ok(part)) = (&full, &part) {
        assert_eq!(part[..], full[..full.len().min(n)]);
    }
});
