//! Go's `TestDecoderReuse` (root package and zlib): large and small streams alternate through one
//! decoder and one reused destination, and each output equals a fresh decode. This crate has no
//! `Decoder`; what outlives a call is the caller's `Vec` and the thread's spare Huffman tables.

mod common;
use common::*;
use deflate::{Error, Limits, zlib};

fn decode(z: bool, data: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let l = Limits::default();
    if z {
        zlib::decode_into(data, out, &l, &NO_CANCEL)
    } else {
        deflate::decode_into(data, out, &l, &NO_CANCEL)
    }
}

#[test]
fn reused_vec_with_alternating_large_and_small_streams() {
    // Every fixture without a dictionary, and the first half of it (an error, with partial output).
    let mut streams = Vec::new();
    for f in fixtures().into_iter().filter(|f| f.dict.is_empty()) {
        let z = f.file.ends_with(".zlib");
        streams.push((f.file.clone(), z, f.data[..f.data.len() / 2].to_vec()));
        streams.push((f.file, z, f.data));
    }
    assert!(streams.len() > 100);
    streams.sort_by_key(|s| s.2.len());
    // Largest, smallest, second largest, second smallest, ...
    let (mut lo, mut hi, mut order) = (0, streams.len(), Vec::new());
    while lo < hi {
        hi -= 1;
        order.push(&streams[hi]);
        if lo < hi {
            order.push(&streams[lo]);
            lo += 1;
        }
    }
    assert_eq!(order.len(), streams.len());
    let mut reused = Vec::new();
    for (name, z, data) in order {
        reused.clear();
        let got = decode(*z, data, &mut reused);
        let mut fresh = Vec::new();
        let want = decode(*z, data, &mut fresh);
        assert_eq!(got, want, "{name} ({} bytes)", data.len());
        assert!(
            reused == fresh,
            "{name}: output differs from a fresh decode"
        );
    }
}
