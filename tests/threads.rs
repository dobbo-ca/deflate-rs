//! The crate spawns no thread: the process thread count (Linux `/proc/self/status`) does not grow
//! before, during (a reader samples it on every refill of its input) or after large decodes and
//! predictor calls. One `#[test]` only, so the harness starts no thread meanwhile.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use deflate::{Limits, predictor, zlib};
use std::io::{self, BufRead, Read};
use std::sync::atomic::{AtomicUsize, Ordering};

fn threads() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The most threads seen at any refill of the input.
static SEEN: AtomicUsize = AtomicUsize::new(0);

/// A `BufRead` over a slice, in 4 KiB steps, that samples the thread count at each refill.
struct Sampling<'a>(&'a [u8]);

impl Read for Sampling<'_> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        let b = self.fill_buf()?;
        let n = b.len().min(p.len());
        p[..n].copy_from_slice(&b[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Sampling<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        SEEN.fetch_max(threads(), Ordering::Relaxed);
        Ok(&self.0[..self.0.len().min(4096)])
    }
    fn consume(&mut self, n: usize) {
        self.0 = &self.0[n..];
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fixtures");
    std::fs::read(format!("{dir}/{name}")).unwrap()
}

#[test]
fn no_threads_are_spawned() {
    let before = threads();
    let l = Limits::default();
    let c = &NO_CANCEL;
    for name in ["tiff-rgb512-noise.zlib", "tiff-rgb512-gradient.zlib"] {
        let z = fixture(name);
        let raw = &z[2..];
        let want = zlib::decode(&z, &l, c).unwrap();
        assert_eq!(want.len(), 786432);
        assert_eq!(threads(), before, "{name} zlib::decode");
        assert_eq!(deflate::decode(raw, &l, c).unwrap(), want);
        assert_eq!(threads(), before, "{name} decode");
        assert_eq!(
            deflate::decode_limit(raw, 1 << 18, &l, c).unwrap().len(),
            1 << 18
        );
        assert_eq!(threads(), before, "{name} decode_limit");

        SEEN.store(0, Ordering::Relaxed);
        let mut out = Vec::new();
        deflate::Reader::new(Sampling(raw), &l, c)
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, want);
        out.clear();
        zlib::Reader::new(Sampling(&z), &l, c)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, want);
        assert!(SEEN.load(Ordering::Relaxed) > 0, "the readers were sampled");
        assert!(
            SEEN.load(Ordering::Relaxed) <= before,
            "{name}: {} threads during a reader, {before} before",
            SEEN.load(Ordering::Relaxed)
        );
        assert_eq!(threads(), before, "{name} readers");
    }

    // The predictor on a 512 x 512 RGB tile (what the decodes above produce).
    let (w, h) = (512, 512);
    let mut src = vec![1u8; w * h * 3];
    let mut dst = vec![0u8; w * h * 4];
    predictor::undo_expand_rgba(&mut dst, &src, w, h, 3, false, &l, c).unwrap();
    predictor::expand_rgba(&mut dst, &src, w, h, 3, false, &l, c).unwrap();
    predictor::undo_horizontal(&mut src, w, h, 3, 1, &l, c).unwrap();
    assert_eq!(threads(), before, "predictor");
}
