//! zlib: header, dictionary id, checksum and the limit rules around them.

mod common;
use common::*;
use deflate::{Error, Limits, adler32, zlib};
use std::io::{self, Read};

/// A zlib stream of `data` as one fixed block, with the right trailer.
fn stream(data: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x9c];
    z.extend(one(|w| {
        w.fixed(true, &lits(data));
    }));
    z.extend_from_slice(
        &adler32::update(1, data, &Limits::default())
            .unwrap()
            .to_be_bytes(),
    );
    z
}

#[test]
fn good_stream() {
    let z = stream(b"hello");
    assert_eq!(unzlib(&z), Ok(b"hello".to_vec()));
    let mut t = z.clone();
    t.extend_from_slice(b"trailing bytes are ignored");
    assert_eq!(unzlib(&t), Ok(b"hello".to_vec()));
    for (cap, p) in [(0, 64), (0, 1), (1, 3)] {
        assert_eq!(read_zlib(&z, &[], cap, p), Ok(b"hello".to_vec()));
    }
}

#[test]
fn header_errors() {
    let l = Limits::default();
    assert_eq!(unzlib(&[]), Err(Error::UnexpectedEof));
    assert_eq!(unzlib(&[0x78]), Err(Error::UnexpectedEof));
    assert!(
        zlib::Reader::new(&[0x78][..], &l, &NO_CANCEL).is_err_and(|e| e == Error::UnexpectedEof)
    );
    // 0x78 0x9c is a valid pair: 0x789c is a multiple of 31.
    for h in [
        [0x78, 0x9d],
        [0x79, 0x00],
        [0x88, 0x00],
        [0x00, 0x00],
        [0xff, 0xff],
    ] {
        let mut z = stream(b"x");
        z[..2].copy_from_slice(&h);
        assert_eq!(unzlib(&z), Err(Error::Header), "{h:x?}");
        assert!(
            zlib::Reader::new(&z[..], &l, &NO_CANCEL).is_err_and(|e| e == Error::Header),
            "{h:x?}"
        );
    }
    // The header is checked before the limit n == 0 returns.
    assert_eq!(
        zlib::decode_limit(&[0x78, 0x9d], 0, &l, &NO_CANCEL),
        Err(Error::Header)
    );
}

#[test]
fn checksum() {
    let mut z = stream(b"hello");
    let n = z.len();
    z[n - 1] ^= 1;
    assert_eq!(unzlib(&z), Err(Error::Checksum));
    assert_eq!(read_zlib(&z, &[], 0, 64), Err(Error::Checksum));
    // The output arrives before the error.
    let mut r = zlib::Reader::new(&z[..], &Limits::default(), &NO_CANCEL).unwrap();
    let mut b = [0u8; 16];
    assert_eq!(std::io::Read::read(&mut r, &mut b).unwrap(), 5);
    assert!(std::io::Read::read(&mut r, &mut b).is_err());
    assert!(std::io::Read::read(&mut r, &mut b).is_err());

    // A stop short of the end skips the trailer; a stop at the end reads it.
    let l = Limits::default();
    assert_eq!(
        zlib::decode_limit(&z, 3, &l, &NO_CANCEL),
        Ok(b"hel".to_vec())
    );
    assert_eq!(
        zlib::decode_limit(&z, 5, &l, &NO_CANCEL),
        Err(Error::Checksum)
    );
    assert_eq!(
        zlib::decode_limit(&z, 6, &l, &NO_CANCEL),
        Err(Error::Checksum)
    );
    assert_eq!(zlib::decode_limit(&z, 0, &l, &NO_CANCEL), Ok(vec![]));
    // No trailer at all.
    assert_eq!(unzlib(&z[..n - 4]), Err(Error::UnexpectedEof));
    assert_eq!(unzlib(&z[..n - 1]), Err(Error::UnexpectedEof));
    assert_eq!(
        zlib::decode_limit(&z[..n - 4], 3, &l, &NO_CANCEL),
        Ok(b"hel".to_vec())
    );
}

#[test]
fn preset_dictionary_id() {
    let l = Limits::default();
    // FDICT set: 0x78bb is a multiple of 31.
    let body = one(|w| {
        w.fixed(true, &lits(b"x"));
    });
    let with_id = |id: u32| {
        let mut z = vec![0x78, 0xbb];
        z.extend_from_slice(&id.to_be_bytes());
        z.extend_from_slice(&body);
        z.extend_from_slice(
            &adler32::update(1, b"x", &Limits::default())
                .unwrap()
                .to_be_bytes(),
        );
        z
    };
    assert_eq!(unzlib(&[0x78, 0xbb, 0, 0, 0]), Err(Error::UnexpectedEof));
    assert_eq!(unzlib(&with_id(0x1234_5678)), Err(Error::Dictionary));
    // Decode has no dictionary, so the id of nothing, 1, is the one it accepts.
    assert_eq!(unzlib(&with_id(1)), Ok(b"x".to_vec()));
    let dict = b"some dictionary";
    let z = with_id(adler32::update(1, dict, &Limits::default()).unwrap());
    assert_eq!(unzlib(&z), Err(Error::Dictionary));
    assert_eq!(read_zlib(&z, dict, 0, 8), Ok(b"x".to_vec()));
    assert!(zlib::Reader::new(&z[..], &l, &NO_CANCEL).is_err_and(|e| e == Error::Dictionary));
}

/// The dictionary is hashed through `adler32::update`: past `max_output` it is `OutputLimit`, not
/// a checksum of the whole thing.
#[test]
fn dict_hash_is_limited() {
    let z = [0x78, 0xbb, 0, 0, 0, 1];
    let l = Limits {
        max_output: 8,
        ..Limits::default()
    };
    let dict = [7u8; 9];
    assert!(
        zlib::Reader::with_dict(&z[..], &dict, &l, &NO_CANCEL)
            .is_err_and(|e| e == Error::OutputLimit)
    );
}

#[test]
fn bad_deflate_body() {
    let mut z = vec![0x78, 0x9c];
    z.extend(one(|w| {
        w.header(true, 3);
    }));
    assert_eq!(unzlib(&z), Err(Error::Corrupt(1)));
    assert_eq!(read_zlib(&z, &[], 0, 8), Err(Error::Corrupt(1)));
}

/// Go zlib exactCapRepro: a stored block cut short.
#[test]
fn exactcap_truncated_stored() {
    let z = b"x\x010\x00 \xff\xdf0";
    assert_eq!(unzlib(z), Err(Error::UnexpectedEof));
    assert_eq!(read_zlib(z, &[], 0, 64), Err(Error::UnexpectedEof));
}

/// A reader that gives `b[..at]`, then `end`: an error, or EOF.
struct TailErr<'a> {
    b: &'a [u8],
    at: usize,
    end: Option<io::ErrorKind>,
}

impl Read for TailErr<'_> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        if self.at == 0 {
            return match self.end {
                Some(k) => Err(io::Error::new(k, "tail")),
                None => Ok(0),
            };
        }
        let n = p.len().min(self.at);
        p[..n].copy_from_slice(&self.b[..n]);
        self.b = &self.b[n..];
        self.at -= n;
        Ok(n)
    }
}

/// Go TestAuditTrailerErrors: the reader fails, or ends, inside the 4-byte trailer. All the
/// output comes first; then the error is the reader's, or an unexpected EOF for an EOF.
#[test]
fn trailer_errors() {
    let text: Vec<u8> = b"some text, some more text. ".repeat(1000);
    let s = stream(&text);
    let kinds = [
        (Some(io::ErrorKind::Other), Error::Io(io::ErrorKind::Other)),
        (None, Error::UnexpectedEof),
        (Some(io::ErrorKind::UnexpectedEof), Error::UnexpectedEof),
    ];
    for at in s.len() - 4..s.len() {
        for (end, want) in &kinds {
            let br = io::BufReader::with_capacity(
                4096,
                TailErr {
                    b: &s,
                    at,
                    end: *end,
                },
            );
            let mut r = zlib::Reader::new(br, &Limits::default(), &NO_CANCEL).unwrap();
            let mut out = Vec::new();
            let e = r.read_to_end(&mut out).unwrap_err();
            assert_eq!(into_error(e), *want, "at {at} end {end:?}");
            assert_eq!(out, text, "at {at} end {end:?}");
        }
    }
}
