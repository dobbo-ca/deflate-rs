#![no_main]
use deflate::{Error, Limits, zlib};
use libfuzzer_sys::fuzz_target;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};

#[path = "../common.rs"]
mod common;

static NO: AtomicBool = AtomicBool::new(false);

/// Reads `r` to the end with a buffer of `size` bytes: the bytes, and the first error if any.
/// `stop` is set before read number `after` (`usize::MAX`: never).
fn drain(
    mut r: impl Read,
    size: usize,
    stop: &AtomicBool,
    after: usize,
) -> (Vec<u8>, Option<Error>) {
    let (mut out, mut buf) = (Vec::new(), vec![0u8; size]);
    let class = |e: std::io::Error| {
        let e = e.get_ref().and_then(|s| s.downcast_ref::<Error>());
        e.cloned().expect("a reader error is an Error")
    };
    for reads in 0.. {
        if reads == after {
            stop.store(true, Ordering::Relaxed);
        }
        match r.read(&mut buf) {
            Ok(0) => {
                // End of stream is sticky.
                assert!(matches!(r.read(&mut buf), Ok(0)), "read after the end");
                return (out, None);
            }
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => {
                // So is an error: the same one again.
                let e = class(e);
                let again = r.read(&mut buf).map_err(class);
                assert_eq!(again.err().as_ref(), Some(&e), "read after an error");
                return (out, Some(e));
            }
        }
    }
    unreachable!()
}

fn read<R: BufRead>(
    src: R,
    zlib: bool,
    dict: &[u8],
    size: usize,
    limits: &Limits,
    stop: &AtomicBool,
    after: usize,
) -> (Vec<u8>, Option<Error>) {
    if zlib {
        match zlib::Reader::with_dict(src, dict, limits, stop) {
            Ok(r) => drain(r, size, stop, after),
            Err(e) => (Vec::new(), Some(e)),
        }
    } else {
        let r = deflate::Reader::with_dict(src, dict, limits, stop);
        drain(r, size, stop, after)
    }
}

// data = [kind][read size - 1: u16 LE][dict length / 160][max_output / 16: u16 LE][dict, stream]. kind:
// bit 0 zlib, bits 1-2 the input (the slice, or a BufReader of 1, 7 or 64 bytes so fill_buf
// returns short chunks), bit 3 takes a dictionary from the front of the rest, bit 4 sets the
// cancel flag: the reader then fails and returns nothing. Bit 5 takes max_alloc from the input
// (0 to 256 KiB, so below what a reader needs too) and bit 6 sets the cancel flag before one
// of the first 16 reads; both read three bytes from the front of the rest first. With bit 5 or
// 6 the reader may stop with AllocLimit or Cancelled, and what it returned is a prefix of the
// whole-buffer decode. The streaming reader, with any read size,
// gives the bytes or the error of the whole-buffer decode. With a dictionary (raw only: the
// whole-buffer zlib decode has none) that decode is of a stored block of the dictionary, then the
// stream, less the dictionary. A zlib reader with a dictionary reads that stream behind a zlib
// header (FDICT) with the dictionary's id, and the Adler-32 of that decode after it, and must give
// the same bytes and error (a bad or missing trailer, the stream being followed by more bytes,
// gives a checksum or an end-of-input error after the same bytes). Every read is read twice at the
// end or at an error: the answer is the same. Bit 7 (raw only)
// puts stored blocks before the stream, from 0 to 4 windows of 32 KiB (plus or minus up to 128
// bytes, from two bytes at the front of the stream), so the reader's window drain and shift fall
// at their edges, with or without a dictionary.
fuzz_target!(|data: &[u8]| {
    common::check_tier();
    let Some((&[kind, size_lo, size_hi, dlen, a, b], rest)) = data.split_first_chunk::<6>() else {
        return;
    };
    // Unless bit 5 says otherwise, max_alloc stays above what a reader needs (about 100 KiB), or
    // it would fail where the whole-buffer decode does not.
    let big = Limits {
        max_output: usize::from(u16::from_le_bytes([a, b])) * 16,
        max_alloc: 1 << 27,
    };
    let mut ex = [0u8; 3];
    let rest = if kind & 0x60 != 0 {
        let n = rest.len().min(3);
        ex[..n].copy_from_slice(&rest[..n]);
        &rest[n..]
    } else {
        rest
    };
    let limits = Limits {
        max_alloc: if kind & 32 != 0 {
            usize::from(u16::from_le_bytes([ex[0], ex[1]])) * 4
        } else {
            big.max_alloc
        },
        ..big
    };
    let after = if kind & 64 != 0 {
        usize::from(ex[2] % 16)
    } else {
        usize::MAX
    };
    if kind & 16 != 0 {
        let stop = AtomicBool::new(true);
        let got = read(rest, kind & 1 != 0, &[], 1, &limits, &stop, usize::MAX);
        assert!(got.0.is_empty() && got.1.is_some());
        return;
    }
    let (z, size) = (
        kind & 1 != 0,
        usize::from(u16::from_le_bytes([size_lo, size_hi])) + 1,
    );
    let dl = if kind & 8 != 0 {
        (usize::from(dlen) * 160).min(rest.len())
    } else {
        0
    };
    let (dict, src) = rest.split_at(dl);
    let stored;
    let src = if kind & 0x81 == 0x80 && src.len() >= 2 {
        let n =
            (usize::from(src[1] % 5) * (1 << 15)).saturating_add_signed(isize::from(src[0] as i8));
        let mut v = Vec::new();
        for i in (0..n).step_by(0xffff) {
            let m = (n - i).min(0xffff) as u16;
            v.extend([0, m as u8, (m >> 8) as u8, !(m as u8), !((m >> 8) as u8)]);
            v.extend((0..m).map(|j| (usize::from(j) + i).wrapping_mul(31) as u8));
        }
        v.extend_from_slice(&src[2..]);
        stored = v;
        &stored[..]
    } else {
        src
    };
    // The reader keeps the last 32 KiB of a dictionary.
    let tail = &dict[dict.len().saturating_sub(1 << 15)..];
    let mut want = Vec::new();
    let r = if z && tail.is_empty() {
        zlib::decode_into(src, &mut want, &big, &NO)
    } else if tail.is_empty() {
        deflate::decode_into(src, &mut want, &big, &NO)
    } else {
        let n = tail.len();
        let pre = [0, n as u8, (n >> 8) as u8, !(n as u8), !((n >> 8) as u8)];
        let full = [&pre[..], tail, src].concat();
        let lim = Limits {
            max_output: big.max_output + n,
            ..big
        };
        let r = deflate::decode_into(&full, &mut want, &lim, &NO);
        want = want.split_off(n.min(want.len()));
        // Offsets count from the start of the stream, here after the stored block.
        r.map_err(|e| match e {
            Error::Corrupt(o) => Error::Corrupt(o.saturating_sub(5 + n as u64)),
            e => e,
        })
    };
    // Whether the raw stream ends the input, so that the trailer follows it exactly.
    let exact = z && !dict.is_empty() && {
        let mut left = src;
        let mut r = deflate::Reader::with_dict(&mut left, dict, &big, &NO);
        let ok = std::io::copy(&mut r, &mut std::io::sink()).is_ok();
        drop(r);
        ok && left.is_empty()
    };
    let zsrc;
    let src = if z && !dict.is_empty() {
        let sum = |p: &[u8]| deflate::adler32::update(1, p, &Limits::default());
        let (id, end) = (sum(dict).expect("limit"), sum(&want).expect("limit"));
        // 0x78 0xbb: FDICT and a valid FCHECK.
        zsrc = [
            &[0x78, 0xbb][..],
            &id.to_be_bytes(),
            src,
            &end.to_be_bytes(),
        ]
        .concat();
        &zsrc[..]
    } else {
        src
    };
    let stop = AtomicBool::new(false);
    let got = match kind >> 1 & 3 {
        0 => read(src, z, dict, size, &limits, &stop, after),
        n => read(
            BufReader::with_capacity([1, 7, 64][n as usize - 1], src),
            z,
            dict,
            size,
            &limits,
            &stop,
            after,
        ),
    };
    // A dictionary longer than max_output fails at the header (it is checksummed first).
    if z && !dict.is_empty() && dict.len() > big.max_output {
        assert!(
            got.0.is_empty() && got.1 == Some(Error::OutputLimit),
            "{:?}",
            got.1
        );
        return;
    }
    // For zlib with a dictionary the reader reads the body and then the trailer bytes, so a body
    // that does not end before them goes on into them: the oracle for that case is the decode of
    // the same bytes (the dictionary's stored block, the body, the trailer), offsets from the body.
    let trailed = (z && !dict.is_empty()).then(|| {
        let n = tail.len();
        let pre = [0, n as u8, (n >> 8) as u8, !(n as u8), !((n >> 8) as u8)];
        // `src` is the zlib stream here: 2 header bytes and the 4-byte id, then the body.
        let full = [&pre[..], tail, &src[6..]].concat();
        let lim = Limits {
            max_output: big.max_output + n,
            ..big
        };
        let mut w = Vec::new();
        let r2 = deflate::decode_into(&full, &mut w, &lim, &NO).map_err(|e| match e {
            Error::Corrupt(o) => Error::Corrupt(o.saturating_sub(5 + n as u64)),
            e => e,
        });
        (w.split_off(n.min(w.len())), r2)
    });
    // A tight max_alloc or a cancel may stop it early: with what it did return a prefix.
    if matches!(
        got.1,
        Some(Error::AllocLimit) if kind & 32 != 0
    ) || matches!(got.1, Some(Error::Cancelled) if kind & 64 != 0)
    {
        let whole = trailed.as_ref().map_or(&want, |t| &t.0);
        assert!(
            whole.starts_with(&got.0),
            "not a prefix of the whole decode"
        );
        return;
    }
    // After a dictionary and a body that decodes, the trailer is what follows the stream.
    if z && !dict.is_empty() && r.is_ok() {
        assert!(
            want == got.0,
            "whole {} bytes, reader {}",
            want.len(),
            got.0.len()
        );
        if exact {
            assert_eq!(got.1, None, "the right trailer follows the stream");
        } else {
            assert!(
                matches!(got.1, None | Some(Error::Checksum | Error::UnexpectedEof)),
                "{:?}",
                got.1
            );
        }
        return;
    }
    // A body that fails: the zlib reader reads on into the trailer bytes after the body, so the
    // oracle is the decode of the same bytes computed above.
    if let Some((w, r2)) = trailed {
        assert!(
            w == got.0,
            "whole {} bytes, reader {}",
            w.len(),
            got.0.len()
        );
        match r2 {
            Err(e) => assert_eq!(Some(e), got.1),
            // The trailer bytes completed the stream, so no trailer is left after it.
            Ok(()) => assert!(
                matches!(got.1, Some(Error::UnexpectedEof | Error::Checksum)),
                "{:?}",
                got.1
            ),
        }
        return;
    }
    // Both give the bytes before the error, as Go does.
    assert!(
        want == got.0,
        "whole {} bytes, reader {}",
        want.len(),
        got.0.len()
    );
    assert_eq!(r.err(), got.1);
});
