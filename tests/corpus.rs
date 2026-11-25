//! `DEFLATE_STREAMS=<file>`: records of `[u32 LE length][zlib stream]...`, for example the tiles
//! of a slide (user-provided data). Never write or log the stream bytes. Skipped when the variable is not set.

mod common;
use common::*;

#[test]
fn corpus() {
    let Ok(path) = std::env::var("DEFLATE_STREAMS") else {
        eprintln!("DEFLATE_STREAMS not set");
        return;
    };
    let b = std::fs::read(path).unwrap();
    let (mut n, mut off, mut output) = (0, 0, 0);
    while off < b.len() {
        assert!(b.len() - off >= 4, "record {n}: bad length");
        let len = u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) as usize;
        assert!(len <= b.len() - off - 4, "record {n}: bad length");
        let rec = &b[off + 4..off + 4 + len];
        off += 4 + len;
        let out = unzlib(rec).unwrap_or_else(|e| panic!("record {n} ({len} bytes): {e}"));
        assert_eq!(inflate(zlib_body(rec)).as_ref(), Ok(&out), "record {n}");
        assert_eq!(
            inflate_limit(zlib_body(rec), out.len()).as_ref(),
            Ok(&out),
            "record {n}"
        );
        assert_eq!(
            read_zlib(rec, &[], 0, 1 << 16).as_ref(),
            Ok(&out),
            "record {n}"
        );
        n += 1;
        output += out.len();
    }
    eprintln!("{n} streams, {off} bytes in, {output} bytes out");
}
