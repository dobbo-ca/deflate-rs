#![no_main]
use deflate::{Limits, zlib};
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::AtomicBool;

#[path = "../common.rs"]
mod common;

// data = [stream][ctl: u8][max_alloc / 64: u16 LE]: the parameters are at the end, so a fixture seed is a
// stream from its first byte. ctl bits 0-1: 4 to 7 threads (fewer is serial); bit 2: zlib;
// bits 3-7: chunks of at least 64 << (that % 7) compressed bytes. The parallel
// decode of one stream gives exactly the serial decode's result and output (with the same prefix
// in `out`), whatever the stream, the threads, the chunk size and a `max_alloc` up to 4 MiB.
fuzz_target!(|data: &[u8]| {
    common::check_tier();
    let Some((src, &[ctl, a, b])) = data.split_last_chunk::<3>() else {
        return;
    };
    let threads = 4 + usize::from(ctl & 3);
    let z = ctl & 4 != 0;
    let chunk = 64 << ((ctl >> 3) % 7);
    let limits = Limits {
        max_output: 1 << 20,
        max_alloc: usize::from(u16::from_le_bytes([a, b])) * 64,
    };
    let stop = AtomicBool::new(false);
    let pre = b"pre".to_vec();
    let (mut s, mut p) = (pre.clone(), pre.clone());
    let (rs, rp) = if z {
        (
            zlib::decode_into(src, &mut s, &limits, &stop),
            zlib::decode_parallel_chunked_into(src, &mut p, &limits, threads, chunk, &stop),
        )
    } else {
        (
            deflate::decode_into(src, &mut s, &limits, &stop),
            deflate::decode_parallel_chunked_into(src, &mut p, &limits, threads, chunk, &stop),
        )
    };
    assert_eq!(rs, rp);
    assert!(s == p, "output differs: {} and {} bytes", s.len(), p.len());
});
