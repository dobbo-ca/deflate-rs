//! The parallel decode of one stream (`decode_parallel_into`, `zlib::...`) gives
//! exactly the serial decode's result and output, for every thread count and chunk size, on valid
//! streams (where the parallel path must run) and on broken ones (where it falls back).

mod common;
use common::*;
use deflate::{Error, Limits, decode_parallel_chunked_into, zlib};

/// The serial and the parallel decode of `src` (zlib if `z`) after `pre`: equal.
fn both(src: &[u8], z: bool, pre: &[u8], l: &Limits, threads: usize, chunk: usize, what: &str) {
    let (mut s, mut p) = (pre.to_vec(), pre.to_vec());
    let (rs, rp) = if z {
        (
            zlib::decode_into(src, &mut s, l, &NO_CANCEL),
            zlib::decode_parallel_chunked_into(src, &mut p, l, threads, chunk, &NO_CANCEL),
        )
    } else {
        (
            deflate::decode_into(src, &mut s, l, &NO_CANCEL),
            decode_parallel_chunked_into(src, &mut p, l, threads, chunk, &NO_CANCEL),
        )
    };
    assert_eq!(rs, rp, "{what}: threads {threads} chunk {chunk}");
    assert!(
        s == p,
        "{what}: threads {threads} chunk {chunk}: output differs"
    );
}

fn streams() -> Vec<(String, bool, Vec<u8>)> {
    fixtures()
        .into_iter()
        .filter(|f| f.dict.is_empty())
        .map(|f| (f.file.clone(), f.file.ends_with(".zlib"), f.data))
        .collect()
}

#[test]
fn fixtures_split_into_small_chunks() {
    let l = Limits::default();
    let before = deflate::parallel_decodes();
    for (name, z, s) in streams() {
        for threads in [2, 3, 4, 8] {
            for chunk in [64, 700, 4096] {
                both(&s, z, &[], &l, threads, chunk, &name);
            }
        }
    }
    // The fixtures with several dynamic blocks (the flush-* ones every 7777 bytes) split. Under 4
    // CPUs every decode is serial (a 2-vCPU hosted runner).
    let ran = deflate::parallel_decodes() - before;
    if std::thread::available_parallelism().map_or(1, |n| n.get()) >= 4 {
        assert!(ran > 50, "the parallel path ran {ran} times");
    }
}

#[test]
fn a_prefix_is_not_history() {
    let l = Limits::default();
    for (name, z, s) in streams() {
        both(&s, z, b"prefix", &l, 4, 512, &name);
    }
}

#[test]
fn mutated_streams_fall_back_to_the_serial_result() {
    let l = Limits::default();
    let mut r = Rng::new(17);
    for (name, z, s) in streams() {
        for i in 0..6 {
            let mut b = s.clone();
            let p = r.below(b.len() * 8);
            b[p / 8] ^= 1 << (p % 8);
            if i % 3 == 2 {
                b.truncate(r.below(b.len()));
            }
            both(&b, z, &[], &l, 4, 256, &format!("{name} ^{p}"));
        }
    }
}

/// Tight limits: the parallel decode holds about three times the output, so it falls back, and
/// the result (success or the limit error) is the serial one.
#[test]
fn limits_are_the_serial_ones() {
    for (name, z, s) in streams() {
        let mut out = Vec::new();
        let ok = if z {
            zlib::decode_into(&s, &mut out, &Limits::default(), &NO_CANCEL)
        } else {
            deflate::decode_into(&s, &mut out, &Limits::default(), &NO_CANCEL)
        };
        if ok.is_err() || out.is_empty() {
            continue;
        }
        let n = out.len();
        for (max_output, max_alloc) in [(n, n), (n - 1, 1 << 30), (1 << 30, n), (1 << 30, 2 * n)] {
            let l = Limits {
                max_output,
                max_alloc,
            };
            both(
                &s,
                z,
                &[],
                &l,
                4,
                512,
                &format!("{name} {max_output} {max_alloc}"),
            );
        }
    }
}

#[test]
fn one_thread_is_serial() {
    let l = Limits::default();
    for (name, z, s) in streams() {
        both(&s, z, &[], &l, 1, 64, &name);
    }
}

#[test]
fn cancel_is_reported() {
    let stop = std::sync::atomic::AtomicBool::new(true);
    let (_, _, s) = streams()
        .into_iter()
        .find(|(n, z, _)| n.contains("flush-full") && !z)
        .unwrap();
    let mut out = Vec::new();
    let r = decode_parallel_chunked_into(&s, &mut out, &Limits::default(), 4, 512, &stop);
    assert_eq!(r, Err(Error::Cancelled));
}
