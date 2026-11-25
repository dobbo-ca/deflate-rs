//! Readers: the window, a preset dictionary, what is consumed from the input, and errors from
//! the input.

mod common;
use common::*;
use deflate::{Error, Limits, zlib};
use std::io::{self, BufRead, BufReader, Read};

/// Stored blocks of `n` bytes of `data`, sizes varied by `n`; the last is final if `fin`.
fn stored_blocks(data: &[u8], fin: bool) -> Vec<u8> {
    let n = data.len();
    let mut out = Vec::new();
    let mut rest = data;
    let mut i = 0;
    while !rest.is_empty() {
        let k = rest.len().min(0xffff - (n * (i + 7)) % 4096);
        let h = u8::from(fin && k == rest.len());
        out.extend_from_slice(&[h, k as u8, (k >> 8) as u8, !(k as u8), !((k >> 8) as u8)]);
        out.extend_from_slice(&rest[..k]);
        rest = &rest[k..];
        i += 1;
    }
    out
}

/// A stored block that fills the Reader's window must not overrun it in the next block, which
/// reads back across the slide with a distance of 32768.
#[test]
fn stored_blocks_across_the_window() {
    const WIN: usize = 128 << 10;
    let tail = [mat(258, 32768), mat(100, 1), mat(50, 4096), lit(b'!')];
    for n in [
        1,
        100,
        WIN - 1,
        WIN,
        WIN + 1,
        WIN - 32768,
        2 * WIN - 32768,
        3 * WIN - 65536,
    ] {
        for dict in [Vec::new(), rand_bytes(3, 32768)] {
            if n < 32768 && dict.is_empty() {
                continue; // no history for the distance
            }
            let data = rand_bytes(n as u64, n);
            for fin in [false, true] {
                let mut s = stored_blocks(&data, fin);
                let mut hist = dict.clone();
                hist.extend_from_slice(&data);
                if !fin {
                    s.extend(one(|w| {
                        w.fixed(true, &tail);
                    }));
                    apply(&mut hist, &tail);
                }
                let want = hist[dict.len()..].to_vec();
                for (cap, p) in [(0, 1), (0, 4096), (0, 32768), (0, 1 << 16), (1, 1 << 16)] {
                    let got = read_raw(&s, &dict, cap, p);
                    assert_eq!(
                        got,
                        Ok(want.clone()),
                        "n {n} dict {} fin {fin} p {p}",
                        dict.len()
                    );
                }
            }
        }
    }
}

#[test]
fn dictionary_is_the_history() {
    let dict = rand_bytes(1, 40960); // only the last 32 KiB count
    let t = [mat(10, 1), mat(20, 32768), mat(7, 5)];
    let s = one(|w| {
        w.fixed(true, &t);
    });
    let mut hist = dict.clone();
    apply(&mut hist, &t);
    assert_eq!(read_raw(&s, &dict, 0, 100), Ok(hist[dict.len()..].to_vec()));
    // Without it there is no history.
    assert!(matches!(read_raw(&s, &[], 0, 100), Err(Error::Corrupt(_))));
    // The dictionary is not output, and a distance past it is an error.
    let far = one(|w| {
        w.fixed(true, &[mat(3, 32768)]);
    });
    assert!(matches!(
        read_raw(&far, &dict[..100], 0, 100),
        Err(Error::Corrupt(_))
    ));
}

/// The reader consumes exactly the stream, so what follows is still in the input.
#[test]
fn stops_at_the_end_of_the_stream() {
    let s = one(|w| {
        w.fixed(false, &lits(b"abc"))
            .stored(false, b"def")
            .fixed(true, &lits(b"ghi"));
    });
    let mut z = vec![0x78, 0x9c];
    z.extend_from_slice(&s);
    z.extend_from_slice(
        &deflate::adler32::update(1, b"abcdefghi", &Limits::default())
            .unwrap()
            .to_be_bytes(),
    );
    let tail = b"next chunk";
    z.extend_from_slice(tail);
    let l = Limits::default();

    let mut input = s.clone();
    input.extend_from_slice(tail);
    let mut src: &[u8] = &input;
    let mut raw = Vec::new();
    deflate::Reader::new(&mut src, &l, &NO_CANCEL)
        .read_to_end(&mut raw)
        .unwrap();
    assert_eq!(raw, b"abcdefghi");
    assert_eq!(src, tail);

    let mut src: &[u8] = &z;
    let mut out = Vec::new();
    deflate::zlib::Reader::new(&mut src, &l, &NO_CANCEL)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, b"abcdefghi");
    assert_eq!(src, tail);
}

/// A reader that fails after `ok` bytes.
struct Failing<'a> {
    ok: &'a [u8],
}

impl Read for Failing<'_> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        if self.ok.is_empty() {
            return Err(io::Error::other("boom"));
        }
        let n = p.len().min(self.ok.len());
        p[..n].copy_from_slice(&self.ok[..n]);
        self.ok = &self.ok[n..];
        Ok(n)
    }
}

#[test]
fn errors_from_the_input_pass_through() {
    let s = one(|w| {
        w.fixed(true, &lits(&rand_bytes(9, 1000)));
    });
    let l = Limits::default();
    for cut in [0, 1, 10, s.len() - 1] {
        let r = BufReader::with_capacity(5, Failing { ok: &s[..cut] });
        assert_eq!(
            read_all(deflate::Reader::new(r, &l, &NO_CANCEL), 64),
            Err(Error::Io(io::ErrorKind::Other)),
            "cut {cut}"
        );
    }
    // The bytes before the error come first, and the error repeats.
    let r = BufReader::with_capacity(5, Failing { ok: &s[..500] });
    let mut r = deflate::Reader::new(r, &l, &NO_CANCEL);
    let mut got = 0;
    let err = loop {
        let mut b = [0u8; 64];
        match r.read(&mut b) {
            Ok(0) => panic!("ended"),
            Ok(n) => got += n,
            Err(e) => break e,
        }
    };
    assert!(got > 0 && got < 1000);
    assert_eq!(into_error(err), Error::Io(io::ErrorKind::Other));
    assert_eq!(
        into_error(r.read(&mut [0u8; 8]).unwrap_err()),
        Error::Io(io::ErrorKind::Other)
    );
}

#[test]
fn eof_and_errors_repeat() {
    let s = one(|w| {
        w.fixed(true, &lits(b"abc"));
    });
    let mut r = deflate::Reader::new(&s[..], &Limits::default(), &NO_CANCEL);
    let mut b = [0u8; 8];
    assert_eq!(r.read(&mut b).unwrap(), 3);
    for _ in 0..3 {
        assert_eq!(r.read(&mut b).unwrap(), 0);
    }
    assert_eq!(r.read(&mut []).unwrap(), 0);

    let bad = one(|w| {
        w.fixed(false, &lits(b"abc")).header(true, 3);
    });
    let mut r = deflate::Reader::new(&bad[..], &Limits::default(), &NO_CANCEL);
    assert_eq!(r.read(&mut b).unwrap(), 3);
    for _ in 0..3 {
        assert!(matches!(
            into_error(r.read(&mut b).unwrap_err()),
            Error::Corrupt(_)
        ));
    }
}

#[test]
fn io_error_kinds() {
    let e: io::Error = Error::UnexpectedEof.into();
    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    let e: io::Error = Error::Corrupt(3).into();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    assert_eq!(into_error(e), Error::Corrupt(3));
}

/// An input whose first `fill_buf` is interrupted, then behaves.
struct Interrupted<'a>(&'a [u8], bool);

impl Read for Interrupted<'_> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        let n = p.len().min(self.0.len());
        p[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

impl BufRead for Interrupted<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if !std::mem::replace(&mut self.1, true) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        Ok(self.0)
    }

    fn consume(&mut self, n: usize) {
        self.0 = &self.0[n..];
    }
}

/// The reader's error is final, so it must not be `Interrupted`: `read_to_end` retries that, and
/// would never return.
#[test]
fn interrupted_input_ends_read_to_end() {
    let s = one(|w| {
        w.fixed(true, &lits(b"abc"));
    });
    let mut r = deflate::Reader::new(Interrupted(&s, false), &Limits::default(), &NO_CANCEL);
    let e = r.read_to_end(&mut Vec::new()).unwrap_err();
    assert_ne!(e.kind(), io::ErrorKind::Interrupted);
    assert_eq!(into_error(e), Error::Io(io::ErrorKind::Interrupted));
    assert!(r.read(&mut [0; 4]).is_err());
}

/// Go's seed exactcap-stored-2000: `ReadAll` gives "0" and the error. `decode` gives only the
/// error; the reader gives the bytes before it.
#[test]
fn reader_returns_output_before_the_error() {
    let s = [0x30, 0x00, 0x20, 0xff, 0xdf, 0x30];
    assert_eq!(inflate(&s), Err(Error::UnexpectedEof));
    let mut out = Vec::new();
    let mut r = deflate::Reader::new(&s[..], &Limits::default(), &NO_CANCEL);
    let e = r.read_to_end(&mut out).unwrap_err();
    assert_eq!(into_error(e), Error::UnexpectedEof);
    assert_eq!(out, b"0");
}

/// Serves `b` in chunks of at most `chunk` bytes. After `at` bytes it fails with `err`, or ends
/// if `err` is `None`.
struct Odd<'a> {
    b: &'a [u8],
    at: usize,
    err: Option<io::ErrorKind>,
    chunk: usize,
}

impl Read for Odd<'_> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        if self.b.is_empty() {
            return Ok(0);
        }
        if self.at == 0 {
            return self.err.map_or(Ok(0), |k| Err(k.into()));
        }
        let n = p.len().min(self.b.len()).min(self.chunk).min(self.at);
        p[..n].copy_from_slice(&self.b[..n]);
        self.b = &self.b[n..];
        self.at -= n;
        Ok(n)
    }
}

/// Reads to the end or the first error: the bytes before it too.
fn read_partial(mut r: impl Read) -> (Vec<u8>, Option<Error>) {
    let (mut out, mut buf) = (Vec::new(), [0u8; 1000]);
    loop {
        match r.read(&mut buf) {
            Ok(0) => return (out, None),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return (out, Some(into_error(e))),
        }
    }
}

/// Go's TestAuditOddReaders, as far as Rust's `Read` goes (it has no `0, nil` retries and no
/// data with an error): inputs that arrive in small chunks and end or fail at some byte. The
/// reader returns the bytes the whole-buffer decode of the same prefix leaves in its `Vec`, then
/// the error; past the end of the stream nothing is read, so the error never shows.
#[test]
fn odd_readers() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fixtures");
    let l = Limits::default();
    for name in ["level6-text", "level0-random", "level1-text"] {
        let z = std::fs::read(format!("{dir}/{name}.zlib")).unwrap();
        let s = zlib_body(&z);
        let s = &s[..s.len() - 4]; // not the Adler-32
        let want = inflate(s).unwrap();
        for at in [0, 1, 2, 5, 100, s.len() / 2, s.len() - 1, s.len()] {
            let mut cut = Vec::new();
            let whole = deflate::decode_into(&s[..at], &mut cut, &l, &NO_CANCEL);
            assert_eq!(whole.is_ok(), at == s.len(), "{name} at {at}");
            for err in [
                None,
                Some(io::ErrorKind::Other),
                Some(io::ErrorKind::UnexpectedEof),
            ] {
                for chunk in [1, 7, 4096, 1 << 20] {
                    for cap in [1, 7, 8192] {
                        let odd = Odd {
                            b: s,
                            at,
                            err,
                            chunk,
                        };
                        let r = BufReader::with_capacity(cap, odd);
                        let (got, e) = read_partial(deflate::Reader::new(r, &l, &NO_CANCEL));
                        let what = format!("{name} at {at} err {err:?} chunk {chunk} cap {cap}");
                        if at == s.len() {
                            assert!(got == want, "{what}");
                            assert_eq!(e, None, "{what}");
                        } else {
                            let want_err = match err {
                                Some(io::ErrorKind::Other) => Error::Io(io::ErrorKind::Other),
                                _ => Error::UnexpectedEof,
                            };
                            assert_eq!(e, Some(want_err), "{what}");
                            assert!(
                                got == cut,
                                "{what}: {} bytes, want {}",
                                got.len(),
                                cut.len()
                            );
                        }
                    }
                }
            }
        }
    }
}

/// As Go's (and the standard library's) reader, a `read` ends at each 32 KiB of the history, also
/// with a dictionary, and returns less than the buffer only there or at the end.
#[test]
fn reads_end_at_each_32k() {
    let data = rand_bytes(5, 100_000);
    let s = stored_blocks(&data, true);
    for dict_len in [0, 100] {
        let dict = rand_bytes(6, dict_len);
        for p in [1000, 40_000, 1 << 20] {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let mut r = deflate::Reader::with_dict(&s[..], &dict, &Limits::default(), &cancel);
            let (mut buf, mut pos) = (vec![0u8; p], dict_len);
            loop {
                let n = r.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                let edge = (pos / 32768 + 1) * 32768 - pos;
                let left = dict_len + data.len() - pos;
                assert_eq!(n, p.min(edge).min(left), "dict {dict_len} p {p} pos {pos}");
                pos += n;
            }
            assert_eq!(pos, dict_len + data.len());
        }
    }
}

/// A sync flush (an empty stored block) ends a `read` as in Go, whatever the buffer.
#[test]
fn reads_end_at_a_sync_flush() {
    let (a, b) = (rand_bytes(7, 1500), rand_bytes(8, 500));
    let mut s = stored_blocks(&a, false);
    s.extend_from_slice(&[0, 0, 0, 0xff, 0xff]);
    s.extend(stored_blocks(&b, true));
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut r = deflate::Reader::new(&s[..], &Limits::default(), &cancel);
    let mut buf = vec![0u8; 4096];
    let sizes: Vec<usize> =
        std::iter::from_fn(|| r.read(&mut buf).ok().filter(|&n| n > 0)).collect();
    assert_eq!(sizes.iter().sum::<usize>(), 2000);
    assert!(sizes.starts_with(&[1500]), "{sizes:?}");
}

/// `read` with an empty buffer returns the stored error once there is one, else `Ok(0)`, as Go's
/// `Read(nil)` (which also runs a decode step; this crate does not).
#[test]
fn zero_length_read() {
    let l = Limits::default();
    let ok = one(|w| {
        w.fixed(true, &lits(b"abc"));
    });
    let bad = [0x07u8]; // a final block of the reserved type 3
    let mut zbad = vec![0x78, 0x9c];
    zbad.extend_from_slice(&bad);
    let mut zsum = vec![0x78, 0x9c];
    zsum.extend_from_slice(&ok);
    zsum.extend_from_slice(&[0, 0, 0, 0]); // not the Adler-32 of "abc"
    let mut buf = [0u8; 8];

    let mut r = deflate::Reader::new(&ok[..], &l, &NO_CANCEL);
    assert_eq!(r.read(&mut []).unwrap(), 0);
    assert_eq!(r.read(&mut buf).unwrap(), 3);
    assert_eq!(r.read(&mut buf).unwrap(), 0);
    assert_eq!(r.read(&mut []).unwrap(), 0);

    let mut r = deflate::Reader::new(&bad[..], &l, &NO_CANCEL);
    assert_eq!(r.read(&mut []).unwrap(), 0);
    let e = into_error(r.read(&mut buf).unwrap_err());
    assert!(matches!(e, Error::Corrupt(_)), "{e:?}");
    assert_eq!(into_error(r.read(&mut []).unwrap_err()), e);

    let mut r = zlib::Reader::new(&zbad[..], &l, &NO_CANCEL).unwrap();
    assert_eq!(r.read(&mut []).unwrap(), 0);
    let e = into_error(r.read(&mut buf).unwrap_err());
    assert!(matches!(e, Error::Corrupt(_)), "{e:?}");
    assert_eq!(into_error(r.read(&mut []).unwrap_err()), e);

    let mut r = zlib::Reader::new(&zsum[..], &l, &NO_CANCEL).unwrap();
    assert_eq!(r.read(&mut buf).unwrap(), 3);
    let e = into_error(r.read(&mut buf).unwrap_err());
    assert_eq!(e, Error::Checksum);
    assert_eq!(into_error(r.read(&mut []).unwrap_err()), e);
}

/// `with_buffer` reads what `new` reads, the same errors included, and `into_buffer` hands the
/// buffer back for a second decode that does not allocate; `max_alloc` limits agree.
#[test]
fn reused_buffer_equals_new() {
    let data = rand_bytes(7, 300_000);
    let ok = stored_blocks(&data, true);
    let mut cut = ok.clone();
    cut.truncate(ok.len() - 1000);
    let mut bad = ok.clone();
    bad[100_000] = 0x07; // a block header with the reserved type
    let big = Vec::with_capacity(1 << 20);
    for src in [&ok, &cut, &bad] {
        for max_alloc in [0, 50_000, 100_000, 150_000, 300_000, 1 << 20, usize::MAX] {
            let l = Limits {
                max_alloc,
                ..Limits::default()
            };
            let want = read_all(deflate::Reader::new(&src[..], &l, &NO_CANCEL), 4096);
            for buf in [Vec::new(), big.clone()] {
                let r = deflate::Reader::with_buffer(&src[..], buf, &l, &NO_CANCEL);
                assert_eq!(read_all(r, 4096), want, "{max_alloc}");
            }
        }
    }
    let l = Limits {
        max_alloc: 0,
        ..Limits::default()
    };
    let r = deflate::Reader::with_buffer(&ok[..], big, &l, &NO_CANCEL);
    assert_eq!(read_all(r, 4096), Err(Error::AllocLimit));

    // zlib
    let mut z = vec![0x78, 0x9c];
    z.extend_from_slice(&ok);
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&(b << 16 | a).to_be_bytes());
    let l = Limits::default();
    let want = read_all(zlib::Reader::new(&z[..], &l, &NO_CANCEL).unwrap(), 4096);
    assert_eq!(want.as_deref(), Ok(&data[..]));
    let r = zlib::Reader::with_buffer(&z[..], Vec::new(), &l, &NO_CANCEL).unwrap();
    assert_eq!(read_all(r, 4096), want);
    assert!(zlib::Reader::with_buffer(&[0u8, 0][..], Vec::new(), &l, &NO_CANCEL).is_err());
}

/// The second decode with the returned buffer keeps its capacity and pointer.
#[test]
fn reused_buffer_does_not_reallocate() {
    let data = rand_bytes(9, 300_000);
    let src = stored_blocks(&data, true);
    let l = Limits::default();
    let mut r = deflate::Reader::with_buffer(&src[..], Vec::new(), &l, &NO_CANCEL);
    io::copy(&mut r, &mut io::sink()).unwrap();
    let buf = r.into_buffer();
    assert!(buf.is_empty() && buf.capacity() > 0);
    let (cap, ptr) = (buf.capacity(), buf.as_ptr());
    let mut r = deflate::Reader::with_buffer(&src[..], buf, &l, &NO_CANCEL);
    io::copy(&mut r, &mut io::sink()).unwrap();
    let buf = r.into_buffer();
    assert_eq!((buf.capacity(), buf.as_ptr()), (cap, ptr));
}
