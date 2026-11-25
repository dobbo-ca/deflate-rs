mod common;
use common::*;
use deflate::{Error, Limits};

fn update(adler: u32, p: &[u8]) -> u32 {
    deflate::adler32::update(adler, p, &Limits::default()).unwrap()
}

/// A slice longer than `max_output` is refused, as for every other entry point.
#[test]
fn update_takes_limits() {
    let l = Limits {
        max_output: 4,
        ..Limits::default()
    };
    assert_eq!(
        deflate::adler32::update(1, b"abcd", &l),
        Ok(update(1, b"abcd"))
    );
    assert_eq!(
        deflate::adler32::update(1, b"abcde", &l),
        Err(Error::OutputLimit)
    );
}

/// The definition: a sum mod 65521 after every byte.
fn naive(adler: u32, p: &[u8]) -> u32 {
    let (mut s1, mut s2) = (u64::from(adler & 0xffff), u64::from(adler >> 16));
    for &x in p {
        s1 = (s1 + u64::from(x)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    ((s2 << 16) | s1) as u32
}

#[test]
fn known_values() {
    assert_eq!(update(1, b""), 1);
    assert_eq!(update(1, b"a"), 0x0062_0062);
    assert_eq!(update(1, b"Wikipedia"), 0x11E6_0398);
    // All 0xff bytes make the sums as big as they get: 5552 is the last length before a mod.
    for n in [5551, 5552, 5553, 11104, 100_000] {
        let p = vec![0xff; n];
        assert_eq!(update(1, &p), naive(1, &p), "n {n}");
    }
}

#[test]
fn lengths_and_offsets() {
    let buf = rand_bytes(1, 20_000 + 64);
    for off in [0, 1, 7, 31, 63] {
        for n in (0..300).chain([
            1023, 1024, 1025, 5551, 5552, 5553, 11103, 11104, 11105, 20_000,
        ]) {
            let p = &buf[off..off + n];
            assert_eq!(update(1, p), naive(1, p), "off {off} n {n}");
        }
    }
}

#[test]
fn resumes_from_any_state() {
    let buf = rand_bytes(2, 30_000);
    let mut r = Rng::new(5);
    for _ in 0..200 {
        let start = ((r.below(65521) as u32) << 16) | r.below(65521) as u32;
        let n = r.below(buf.len());
        assert_eq!(update(start, &buf[..n]), naive(start, &buf[..n]));
        let k = r.below(n + 1);
        assert_eq!(
            update(update(start, &buf[..k]), &buf[k..n]),
            update(start, &buf[..n])
        );
    }
}
