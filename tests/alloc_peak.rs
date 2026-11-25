//! `max_alloc` is the peak of one call: a counting global allocator measures the bytes live above
//! the baseline at the start of each call, for several `max_alloc` values, and each call either
//! fails with `AllocLimit`/`OutputLimit` or stays within `max_alloc` plus a small fixed allowance.
//! Where the code guarantees it, the call must succeed: a one-shot decode at `max_alloc` equal to
//! its output (`Out::reserve` never asks for more than the output, and the fast loop's extra room
//! is requested only if it fits, else the careful path finishes), a reader at 256 KiB (its buffer
//! is at most about 96 KiB: three windows).
//!
//! The allocator counts a `realloc` by its net growth, so the moment a growing `Vec` that cannot
//! be resized in place holds the old block and the new one is not counted. That is the one
//! exception to the peak (allocator-dependent; glibc moves large blocks with `mremap`).
//!
//! One `#[test]` only, so the harness runs no other thread that allocates meanwhile.

mod common;
use common::*;
use deflate::{Error, Limits, zlib};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Bytes live now, and the most live since the last `measure` began.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// What the measured window allocates beyond the charged buffer: the `io::Error` boxes of a
/// failed reader (under 200 bytes), the one-time `DEFLATE_SIMD` lookup, and the links of the
/// Huffman tables if a block needs more than the warm-up did. The Huffman tables themselves
/// (about 35 KB) are in the thread-local spare from the warm-up, so they are not here. 16 KiB
/// is far above that sum and far below the 64 KiB budget: an uncharged buffer shows.
const ALLOWANCE: usize = 16 << 10;

struct Counting;

fn grow(n: usize) {
    let now = LIVE.fetch_add(n, Relaxed) + n;
    PEAK.fetch_max(now, Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's arguments; it only adds counters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract, which `System` shares.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc_zeroed`'s contract, which `System` shares.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            grow(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` and `l` are from an earlier call on this allocator, i.e. on `System`.
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        // SAFETY: `p` and `l` are from an earlier call on this allocator, and the caller
        // upholds `realloc`'s contract for `new`.
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                grow(new - l.size());
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Runs `f` and returns its result and the peak of live bytes above those live at the start.
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    let r = f();
    (r, PEAK.load(Relaxed).saturating_sub(base))
}

fn fixture(name: &str) -> Vec<u8> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fixtures");
    std::fs::read(format!("{dir}/{name}")).unwrap()
}

/// Reads `r` to the end through `buf`: the byte count, or the first error.
fn drain(mut r: impl Read, buf: &mut [u8]) -> Result<usize, Error> {
    let mut n = 0;
    loop {
        match r.read(buf) {
            Ok(0) => return Ok(n),
            Ok(k) => n += k,
            Err(e) => return Err(into_error(e)),
        }
    }
}

/// Runs one call and checks it: the result is as expected or a limit error, and the peak is
/// within `max_alloc` plus the allowance. From `must` on, the call must succeed.
fn check(
    ctx: &str,
    max_alloc: usize,
    expect: usize,
    must: usize,
    f: impl FnOnce() -> Result<usize, Error>,
) {
    let (r, peak) = measure(f);
    let ctx = format!("{ctx} max_alloc={max_alloc}: peak {peak}, result {r:?}");
    assert!(peak <= max_alloc + ALLOWANCE, "{ctx}");
    match r {
        Ok(n) => assert_eq!(n, expect, "{ctx}"),
        Err(Error::AllocLimit | Error::OutputLimit) => assert!(max_alloc < must, "{ctx}"),
        Err(_) => panic!("{ctx}"),
    }
}

#[test]
fn peak_stays_within_max_alloc() {
    let mut buf = vec![0u8; 32 << 10];
    let parallel_before = deflate::parallel_decodes();
    // Noise (literals, long codes) and a smooth image (long matches): both 768 KiB of output.
    for name in ["tiff-rgb512-noise.zlib", "tiff-rgb512-gradient.zlib"] {
        let z = fixture(name);
        let raw = &z[2..];
        let want = unzlib(&z).unwrap().len();
        assert_eq!(want, 786432);
        let part = want - 1000;
        // Warm up: the decoder's thread-local tables exist before the baseline.
        assert_eq!(inflate(raw).unwrap().len(), want);

        for max_alloc in [
            64 << 10,
            256 << 10,
            want / 2,
            want,
            2 * want,
            4 * want,
            8 * want,
        ] {
            let l = &Limits {
                max_alloc,
                ..Limits::default()
            };
            let c = &NO_CANCEL;
            let ck = |op: &str, expect, must, f: &mut dyn FnMut() -> Result<usize, Error>| {
                check(&format!("{name} {op}"), max_alloc, expect, must, f)
            };
            ck("decode", want, want, &mut || {
                deflate::decode(raw, l, c).map(|v| v.len())
            });
            ck("decode_into", want, want, &mut || {
                let mut v = Vec::new();
                deflate::decode_into(raw, &mut v, l, c).map(|()| v.len())
            });
            ck("zlib::decode", want, want, &mut || {
                zlib::decode(&z, l, c).map(|v| v.len())
            });
            ck("decode_limit", part, part, &mut || {
                deflate::decode_limit(raw, part, l, c).map(|v| v.len())
            });
            ck("Reader", want, 256 << 10, &mut || {
                drain(deflate::Reader::new(raw, l, c), &mut buf)
            });
            ck("zlib::Reader", want, 256 << 10, &mut || {
                drain(zlib::Reader::new(&z[..], l, c)?, &mut buf)
            });
            // The reusable decoder, and the parallel decode of one stream (4 and 8
            // threads, 4 KiB chunks so this stream splits): every worker's spans and tables, the
            // output and the windows are charged, and a budget that does not hold them falls back
            // to the serial decode, which must then succeed at `want`.
            ck("Decoder", want, want, &mut || {
                let mut v = Vec::new();
                deflate::Decoder::new()
                    .decode_into(raw, &mut v, l, c)
                    .map(|()| v.len())
            });
            for threads in [4, 8] {
                ck(
                    &format!("decode_parallel {threads}"),
                    want,
                    want,
                    &mut || {
                        let mut v = Vec::new();
                        deflate::decode_parallel_chunked_into(raw, &mut v, l, threads, 4096, c)
                            .map(|()| v.len())
                    },
                );
                ck(
                    &format!("zlib::decode_parallel {threads}"),
                    want,
                    want,
                    &mut || {
                        let mut v = Vec::new();
                        zlib::decode_parallel_chunked_into(&z, &mut v, l, threads, 4096, c)
                            .map(|()| v.len())
                    },
                );
            }
        }
    }
    // At 8 times the output the parallel decode of the noise stream fits its budget (spans grown by
    // doubling, the output, the windows, the tables: about 4 to 5 times the output), so the
    // success path was measured too, on a runner with 4 CPUs or more.
    if std::thread::available_parallelism().map_or(1, |n| n.get()) >= 4 {
        assert!(deflate::parallel_decodes() > parallel_before);
    }
}
