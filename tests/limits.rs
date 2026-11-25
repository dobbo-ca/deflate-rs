//! Limits and cancellation. Go had neither; every call is bounded here.

mod common;
use common::*;
use deflate::{Error, Limits, zlib};
use std::io::{self, BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};

fn fixture(name: &str) -> Vec<u8> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fixtures");
    std::fs::read(format!("{dir}/{name}")).unwrap()
}

/// 84 bytes that inflate to 64 KiB of zeros.
const ZEROS: &str = "level9-zeros.zlib";

#[test]
fn max_output() {
    let z = fixture(ZEROS);
    let at = |n| Limits {
        max_output: n,
        ..Limits::default()
    };
    assert_eq!(
        deflate::zlib::decode(&z, &at(65536), &NO_CANCEL).map(|v| v.len()),
        Ok(65536)
    );
    assert_eq!(
        deflate::zlib::decode(&z, &at(65535), &NO_CANCEL),
        Err(Error::OutputLimit)
    );
    assert_eq!(
        deflate::zlib::decode(&z, &at(0), &NO_CANCEL),
        Err(Error::OutputLimit)
    );
    let raw = zlib_body(&z);
    assert_eq!(
        deflate::decode(raw, &at(65535), &NO_CANCEL),
        Err(Error::OutputLimit)
    );
    // A stop at n comes first when n is under the limit.
    assert_eq!(
        deflate::decode_limit(raw, 100, &at(100), &NO_CANCEL).map(|v| v.len()),
        Ok(100)
    );
    assert_eq!(
        deflate::decode_limit(raw, 101, &at(100), &NO_CANCEL),
        Err(Error::OutputLimit)
    );
    let r = deflate::Reader::new(raw, &at(65535), &NO_CANCEL);
    assert_eq!(read_all(r, 4096), Err(Error::OutputLimit));
    let r = deflate::Reader::new(raw, &at(65536), &NO_CANCEL);
    assert_eq!(read_all(r, 4096).map(|v| v.len()), Ok(65536));
}

#[test]
fn max_alloc() {
    let z = fixture(ZEROS);
    let at = |n| Limits {
        max_alloc: n,
        ..Limits::default()
    };
    assert_eq!(
        deflate::zlib::decode(&z, &at(65536), &NO_CANCEL).map(|v| v.len()),
        Ok(65536)
    );
    assert_eq!(
        deflate::zlib::decode(&z, &at(65535), &NO_CANCEL),
        Err(Error::AllocLimit)
    );
    assert_eq!(
        deflate::zlib::decode(&z, &at(1000), &NO_CANCEL),
        Err(Error::AllocLimit)
    );
    // A stored block of 65535 bytes asks for its room before it reads the data.
    let s = one(|w| {
        w.stored(true, &[7; 65535]);
    });
    assert_eq!(
        deflate::decode(&s, &at(65534), &NO_CANCEL),
        Err(Error::AllocLimit)
    );
    assert_eq!(
        deflate::decode(&s, &at(65535), &NO_CANCEL).map(|v| v.len()),
        Ok(65535)
    );
}

#[test]
fn cancel_is_checked_at_each_block() {
    let stop = AtomicBool::new(true);
    let l = Limits::default();
    let s = one(|w| {
        w.fixed(true, &lits(b"abc"));
    });
    assert_eq!(deflate::decode(&s, &l, &stop), Err(Error::Cancelled));
    assert_eq!(
        deflate::decode_limit(&s, 2, &l, &stop),
        Err(Error::Cancelled)
    );
    assert_eq!(
        deflate::zlib::decode(&fixture(ZEROS), &l, &stop),
        Err(Error::Cancelled)
    );
    // Cleared again, the same call decodes.
    stop.store(false, Ordering::Relaxed);
    assert_eq!(deflate::decode(&s, &l, &stop), Ok(b"abc".to_vec()));
}

/// One Huffman block of 129 MB: a flag set while it decodes stops it, not at the next block.
#[test]
fn cancel_is_checked_inside_a_block() {
    let mut toks = lits(b"a");
    toks.resize(500_000, mat(258, 1));
    let s = one(|w| {
        w.fixed(true, &toks);
    });
    let stop = AtomicBool::new(false);
    let r = std::thread::scope(|sc| {
        sc.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            stop.store(true, Ordering::Relaxed);
        });
        deflate::decode(&s, &Limits::default(), &stop)
    });
    assert_eq!(r, Err(Error::Cancelled));
}

#[test]
fn defaults_are_finite() {
    let l = Limits::default();
    assert!(l.max_output < usize::MAX && l.max_alloc < usize::MAX);
}

/// An input that never ends: `prefix`, then `02 08 20 80 00` forever. That is four empty fixed
/// blocks, so a reader over it makes no output and never returns on its own. The flag is set
/// once `limit` bytes have been consumed.
struct Endless<'a> {
    buf: Vec<u8>,
    pre: usize,
    off: usize,
    served: usize,
    limit: usize,
    stop: &'a AtomicBool,
}

impl<'a> Endless<'a> {
    fn new(prefix: &[u8], limit: usize, stop: &'a AtomicBool) -> Self {
        let mut buf = prefix.to_vec();
        for _ in 0..1000 {
            buf.extend_from_slice(&[0x02, 0x08, 0x20, 0x80, 0x00]);
        }
        Self {
            buf,
            pre: prefix.len(),
            off: 0,
            served: 0,
            limit,
            stop,
        }
    }
}

impl Read for Endless<'_> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        let b = self.fill_buf()?;
        let n = b.len().min(p.len());
        p[..n].copy_from_slice(&b[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Endless<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Ok(&self.buf[self.off..])
    }

    fn consume(&mut self, n: usize) {
        self.off += n;
        if self.off >= self.buf.len() {
            self.off = self.pre + (self.off - self.buf.len()) % (self.buf.len() - self.pre);
        }
        self.served += n;
        if self.served > self.limit {
            self.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// One `read` over empty blocks never returns output, so it must check the flag per block.
#[test]
fn reader_is_cancelled_in_empty_blocks() {
    let l = Limits::default();
    let stop = AtomicBool::new(false);
    let mut r = deflate::Reader::new(Endless::new(&[], 1 << 22, &stop), &l, &stop);
    let e = r.read(&mut [0u8; 64]).unwrap_err();
    assert_ne!(e.kind(), io::ErrorKind::Interrupted);
    assert_eq!(into_error(e), Error::Cancelled);
    assert!(stop.load(Ordering::Relaxed));
    // The error is final.
    assert_eq!(
        into_error(r.read(&mut [0u8; 64]).unwrap_err()),
        Error::Cancelled
    );

    // read_to_end gives up on it too (it retries Interrupted).
    let stop = AtomicBool::new(false);
    let r = deflate::Reader::new(Endless::new(&[], 1 << 20, &stop), &l, &stop);
    assert_eq!(read_all(r, 64), Err(Error::Cancelled));

    let stop = AtomicBool::new(false);
    let r = zlib::Reader::new(Endless::new(&[0x78, 0x9c], 1 << 22, &stop), &l, &stop).unwrap();
    assert_eq!(read_all(r, 64), Err(Error::Cancelled));
}

/// One Huffman block of 129 MB read through a `Reader`: a step ends every 32 KiB, so the flag
/// must be checked when the block resumes, not only per 64 KiB written inside one call.
#[test]
fn reader_cancel_is_checked_inside_a_block() {
    let mut toks = lits(b"a");
    toks.resize(500_000, mat(258, 1));
    let raw = one(|w| {
        w.fixed(true, &toks);
    });
    let z = [&[0x78, 0x9c][..], &raw].concat();
    for zl in [false, true] {
        let stop = AtomicBool::new(false);
        let l = Limits::default();
        let r = std::thread::scope(|sc| {
            sc.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                stop.store(true, Ordering::Relaxed);
            });
            if zl {
                read_all(zlib::Reader::new(&z[..], &l, &stop).unwrap(), 4096)
            } else {
                read_all(deflate::Reader::new(&raw[..], &l, &stop), 4096)
            }
        });
        assert_eq!(r, Err(Error::Cancelled), "zlib: {zl}");
    }
}

/// Set before the first read: no input is needed.
#[test]
fn reader_checks_the_flag_before_reading() {
    let stop = AtomicBool::new(true);
    let s = one(|w| {
        w.fixed(true, &lits(b"abc"));
    });
    let r = deflate::Reader::new(&s[..], &Limits::default(), &stop);
    assert_eq!(read_all(r, 64), Err(Error::Cancelled));
}

/// A dynamic block decoded inside a thread-local's `Drop`, after the decoder's own spare-table
/// thread-local was destroyed, must not panic.
#[test]
fn decode_in_tls_drop() {
    struct D;
    impl Drop for D {
        fn drop(&mut self) {
            let s = one(|w| {
                w.dynamic(true, &lengths(257, &[(256, 1)]), &[0u8], &[]);
            });
            assert_eq!(inflate(&s), Ok(vec![]));
        }
    }
    thread_local!(static D1: D = const { D });
    std::thread::spawn(|| {
        // D1 first, so it is destroyed after the spare tables that the decode below creates.
        D1.with(|_| ());
        let s = one(|w| {
            w.dynamic(true, &lengths(257, &[(256, 1)]), &[0u8], &[]);
        });
        assert_eq!(inflate(&s), Ok(vec![]));
    })
    .join()
    .unwrap();
}
