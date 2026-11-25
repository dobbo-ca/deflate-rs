//! Go returns the output with the error (`io.ReadAll` semantics). The `*_into` functions leave
//! it in the caller's `Vec`.

mod common;
use common::*;
use deflate::{Error, Limits, zlib};

fn l() -> Limits {
    Limits::default()
}

#[test]
fn raw_error_keeps_the_output_before_it() {
    let s = one(|w| {
        w.fixed(false, &lits(b"abc"))
            .stored(false, b"def")
            .header(true, 3);
    });
    // Appended to what is there.
    let mut out = b"xy".to_vec();
    let e = deflate::decode_into(&s, &mut out, &l(), &NO_CANCEL);
    assert!(matches!(e, Err(Error::Corrupt(_))), "{e:?}");
    assert_eq!(out, b"xyabcdef");
    let mut out = Vec::new();
    let e = deflate::decode_limit_into(&s, 100, &mut out, &l(), &NO_CANCEL);
    assert!(matches!(e, Err(Error::Corrupt(_))), "{e:?}");
    assert_eq!(out, b"abcdef");
    // The wrappers return the error only.
    assert!(matches!(inflate(&s), Err(Error::Corrupt(_))));
}

#[test]
fn truncated_stored_block_keeps_the_bytes_there() {
    let s = one(|w| {
        w.stored(true, b"abcdef");
    });
    let mut out = Vec::new();
    let e = deflate::decode_into(&s[..s.len() - 3], &mut out, &l(), &NO_CANCEL);
    assert_eq!(e, Err(Error::UnexpectedEof));
    assert_eq!(out, b"abc");
}

#[test]
fn success_appends() {
    let s = one(|w| {
        w.fixed(true, &lits(b"abc"));
    });
    let mut out = b"xy".to_vec();
    assert_eq!(deflate::decode_into(&s, &mut out, &l(), &NO_CANCEL), Ok(()));
    assert_eq!(out, b"xyabc");
    assert_eq!(
        deflate::decode_limit_into(&s, 2, &mut out, &l(), &NO_CANCEL),
        Ok(())
    );
    assert_eq!(out, b"xyabcab");
}

/// What was in `out` before is not history, as `dst` in Go: a match into it is corrupt.
#[test]
fn earlier_bytes_are_not_history() {
    let s = one(|w| {
        w.fixed(true, &[mat(3, 1)]);
    });
    let mut out = b"abcdef".to_vec();
    let e = deflate::decode_into(&s, &mut out, &l(), &NO_CANCEL);
    assert!(matches!(e, Err(Error::Corrupt(_))), "{e:?}");
    assert_eq!(out, b"abcdef");
}

/// The limits count from where the output starts.
#[test]
fn limits_do_not_count_earlier_bytes() {
    let s = one(|w| {
        w.stored(true, &[7; 100]);
    });
    let mut out = vec![1; 1000];
    let tight = Limits {
        max_output: 100,
        max_alloc: 100,
    };
    assert_eq!(
        deflate::decode_into(&s, &mut out, &tight, &NO_CANCEL),
        Ok(())
    );
    assert_eq!(out.len(), 1100);
    let tight = Limits {
        max_output: 99,
        ..tight
    };
    let mut out = vec![1; 1000];
    assert_eq!(
        deflate::decode_into(&s, &mut out, &tight, &NO_CANCEL),
        Err(Error::OutputLimit)
    );
    assert_eq!(out.len(), 1099);
}

#[test]
fn zlib_checksum_error_keeps_all_the_output() {
    let mut z = vec![0x78, 0x9c];
    z.extend(one(|w| {
        w.fixed(true, &lits(b"hello"));
    }));
    z.extend_from_slice(&[0, 0, 0, 1]);
    let mut out = Vec::new();
    assert_eq!(
        zlib::decode_into(&z, &mut out, &l(), &NO_CANCEL),
        Err(Error::Checksum)
    );
    assert_eq!(out, b"hello");
    // A stop at n does not read the trailer.
    let mut out = Vec::new();
    assert_eq!(
        zlib::decode_limit_into(&z, 3, &mut out, &l(), &NO_CANCEL),
        Ok(())
    );
    assert_eq!(out, b"hel");
    // A bad header gives nothing.
    let mut out = b"xy".to_vec();
    assert_eq!(
        zlib::decode_into(&[0x78, 0x9d, 0], &mut out, &l(), &NO_CANCEL),
        Err(Error::Header)
    );
    assert_eq!(out, b"xy");
    // The wrapper is the error only.
    assert_eq!(unzlib(&z), Err(Error::Checksum));
}

#[test]
fn zlib_corrupt_body_keeps_the_output_before_it() {
    let mut z = vec![0x78, 0x9c];
    z.extend(one(|w| {
        w.fixed(false, &lits(b"abc")).header(true, 3);
    }));
    let mut out = Vec::new();
    let e = zlib::decode_into(&z, &mut out, &l(), &NO_CANCEL);
    assert!(matches!(e, Err(Error::Corrupt(_))), "{e:?}");
    assert_eq!(out, b"abc");
}
