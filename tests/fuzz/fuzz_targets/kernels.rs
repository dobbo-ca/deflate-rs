#![no_main]
//! The differential target of the unsafe kernels against their safe references: the decode loop
//! (raw-pointer writes into the spare capacity of `out`) against the careful path alone, and each
//! fused copy + Adler-32 kernel this CPU has against a plain copy and the definition.
use deflate::{Limits, adler32};
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::AtomicBool;

#[path = "../common.rs"]
mod common;

/// The definition of Adler-32, one byte at a time.
fn reference(adler: u32, p: &[u8]) -> u32 {
    let (mut s1, mut s2) = (u64::from(adler & 0xffff), u64::from(adler >> 16));
    for &x in p {
        s1 = (s1 + u64::from(x)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    ((s2 << 16) | s1) as u32
}

// data = [stream][pre: u8][adler: u32 LE]: the parameters are at the end, so a fixture seed is a
// stream from its first byte. The stream decodes into `out` holding `pre` bytes (the loop runs
// with history below it) both ways: the same result and the same bytes. Then every copy kernel
// appends the stream's bytes from start `pre % 64` to a prefix of `pre % 61` bytes (any
// alignment of both), from the initial value `adler`.
fuzz_target!(|data: &[u8]| {
    common::check_tier();
    let Some((src, &[pre, a, b, c, d])) = data.split_last_chunk::<5>() else {
        return;
    };
    let limits = Limits {
        max_output: 1 << 20,
        max_alloc: 1 << 22,
    };
    let stop = AtomicBool::new(false);
    let prefix: Vec<u8> = (0..pre).collect();
    let (mut fast, mut slow) = (prefix.clone(), prefix.clone());
    let rf = deflate::decode_into(src, &mut fast, &limits, &stop);
    let rs = deflate::decode_careful_into(src, usize::MAX, &mut slow, &limits, &stop);
    assert_eq!(rf, rs);
    assert!(
        fast == slow,
        "output differs: {} and {} bytes",
        fast.len(),
        slow.len()
    );

    let adler = u32::from_le_bytes([a, b, c, d]);
    let p = &src[usize::from(pre % 64).min(src.len())..];
    let prefix = &prefix[..usize::from(pre % 61)];
    let want = reference(adler, p);
    for (name, out, sum) in adler32::copy_sum_tiers(prefix, p, adler) {
        assert_eq!(sum, want, "{name}");
        assert!(
            out.starts_with(prefix) && out[prefix.len()..] == *p,
            "{name}: bytes differ"
        );
    }
});
