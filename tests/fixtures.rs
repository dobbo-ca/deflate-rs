//! The Go fixtures (`tests/data/fixtures`, written by zlib 1.2.11 at every level, strategy, wbits
//! and memLevel, with flushes, preset dictionaries and PNG and TIFF tiles). Go compares them with
//! compress/flate at run time; here every stream of one input must decode to the same bytes of
//! the manifest's length, and the zlib ones check their own Adler-32.

mod common;
use common::*;
use deflate::{Error, Limits};
use std::collections::HashMap;

fn is_zlib(f: &Fixture) -> bool {
    f.file.ends_with(".zlib")
}

/// Decodes a fixture through the whole-buffer API.
fn decode(f: &Fixture) -> Result<Vec<u8>, Error> {
    match (is_zlib(f), f.dict.is_empty()) {
        (true, true) => unzlib(&f.data),
        (true, false) => read_zlib(&f.data, &f.dict, 0, 4096),
        (false, true) => inflate(&f.data),
        (false, false) => read_raw(&f.data, &f.dict, 0, 4096),
    }
}

#[test]
fn every_stream_of_an_input_gives_the_same_bytes() {
    let mut seen: HashMap<(String, usize), Vec<u8>> = HashMap::new();
    let all = fixtures();
    assert!(all.len() > 100);
    for f in &all {
        let out = decode(f).unwrap_or_else(|e| panic!("{}: {e}", f.file));
        assert_eq!(out.len(), f.insize, "{}", f.file);
        // Each tile and PNG stream has its own input.
        let key = if f.input.starts_with("tiff") || f.input.starts_with("png") {
            f.file.split('.').next().unwrap().to_string()
        } else {
            f.input.clone()
        };
        let first = seen.entry((key, f.insize)).or_insert_with(|| out.clone());
        assert!(
            *first == out,
            "{}: differs from the other streams of {}",
            f.file,
            f.input
        );
    }
}

#[test]
fn dictionary_streams_need_their_dictionary() {
    for f in fixtures()
        .iter()
        .filter(|f| !f.dict.is_empty() && is_zlib(f))
    {
        assert_eq!(unzlib(&f.data), Err(Error::Dictionary), "{}", f.file);
        assert_eq!(
            read_zlib(&f.data, &[], 0, 64),
            Err(Error::Dictionary),
            "{}",
            f.file
        );
        // The wrong dictionary has another Adler-32.
        assert_eq!(
            read_zlib(&f.data, b"nope", 0, 64),
            Err(Error::Dictionary),
            "{}",
            f.file
        );
    }
}

#[test]
fn readers_equal_decode() {
    for f in fixtures().iter().filter(|f| f.dict.is_empty()) {
        let want = decode(f);
        for (cap, bufsz) in [(0, 1 << 16), (0, 1), (1, 4093), (16, 32769)] {
            let got = if is_zlib(f) {
                read_zlib(&f.data, &[], cap, bufsz)
            } else {
                read_raw(&f.data, &[], cap, bufsz)
            };
            assert_eq!(got, want, "{} cap {cap} buf {bufsz}", f.file);
        }
    }
}

#[test]
fn decode_limit_is_a_prefix() {
    for f in fixtures().iter().filter(|f| f.dict.is_empty()) {
        let full = decode(f).unwrap();
        let l = full.len();
        for n in [
            0,
            1,
            100,
            32767,
            32768,
            32769,
            65536,
            l.saturating_sub(1),
            l,
            l + 1,
        ] {
            let want = full[..n.min(l)].to_vec();
            let got = if is_zlib(f) {
                deflate::zlib::decode_limit(&f.data, n, &Limits::default(), &NO_CANCEL)
            } else {
                inflate_limit(&f.data, n)
            };
            assert_eq!(got, Ok(want), "{} n={n}", f.file);
        }
    }
}

/// Every cut of a complete stream is an unexpected EOF, wherever it falls.
#[test]
fn truncations_are_unexpected_eof() {
    for f in fixtures().iter().filter(|f| f.dict.is_empty()) {
        let n = f.data.len();
        let step = (n / 64).max(1);
        let cuts = (0..n.min(40))
            .chain((0..n).step_by(step))
            .chain(n.saturating_sub(40)..n);
        for i in cuts {
            let got = if is_zlib(f) {
                unzlib(&f.data[..i])
            } else {
                inflate(&f.data[..i])
            };
            assert_eq!(got, Err(Error::UnexpectedEof), "{} cut at {i}", f.file);
        }
    }
}

/// Flipped bits: no panic, and the readers agree with the whole-buffer decode, error and offset
/// included.
#[test]
fn bit_flips_agree() {
    let mut r = Rng::new(7);
    for f in fixtures().iter().filter(|f| f.dict.is_empty()) {
        let mut b = f.data.clone();
        for _ in 0..24 {
            let bit = r.below(8 * b.len());
            b[bit / 8] ^= 1 << (bit % 8);
            let (want, got) = if is_zlib(f) {
                (unzlib(&b), read_zlib(&b, &[], 0, 4096))
            } else {
                (inflate(&b), read_raw(&b, &[], 0, 4096))
            };
            assert_eq!(got, want, "{} flip bit {bit}", f.file);
            if let (Ok(full), false) = (&want, is_zlib(f)) {
                let n = r.below(full.len() + 2);
                assert_eq!(inflate_limit(&b, n), Ok(full[..n.min(full.len())].to_vec()));
            }
            b[bit / 8] ^= 1 << (bit % 8);
        }
    }
}
