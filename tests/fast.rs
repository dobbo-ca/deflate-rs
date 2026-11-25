//! The fast loop against the careful path alone, which is a port of Go's: the same output, the
//! same error with the same offset, the same output before an error. The `parity` job checks the
//! same against Go itself; this one needs no Go and runs on every target.

mod common;
use common::*;
use deflate::{Limits, decode_careful_into};

/// Decodes `src` (at most `n` bytes) both ways and compares everything.
fn same(src: &[u8], n: usize, what: &str) {
    let l = Limits::default();
    let (mut fast, mut slow) = (Vec::new(), Vec::new());
    let rf = if n == usize::MAX {
        deflate::decode_into(src, &mut fast, &l, &NO_CANCEL)
    } else {
        deflate::decode_limit_into(src, n, &mut fast, &l, &NO_CANCEL)
    };
    let rs = decode_careful_into(src, n, &mut slow, &l, &NO_CANCEL);
    assert_eq!(rf, rs, "{what}, n {n}");
    assert!(fast == slow, "{what}, n {n}: output differs");
}

fn streams() -> Vec<(String, Vec<u8>)> {
    fixtures()
        .into_iter()
        .map(|f| {
            let z = f.file.ends_with(".zlib");
            let body = if z { zlib_body(&f.data) } else { &f.data[..] };
            (f.file, body.to_vec())
        })
        .collect()
}

#[test]
fn fixtures_and_their_limits() {
    for (name, s) in streams() {
        same(&s, usize::MAX, &name);
        for n in [1000, 32768, 70000] {
            same(&s, n, &name);
        }
    }
}

#[test]
fn mutated_fixtures() {
    let mut r = Rng::new(7);
    for (name, s) in streams() {
        if s.len() > 20_000 {
            continue;
        }
        for i in 0..4 {
            let mut b = s.clone();
            let p = r.below(b.len() * 8);
            b[p / 8] ^= 1 << (p % 8);
            if i % 3 == 2 {
                b.truncate(r.below(b.len()));
            }
            same(&b, usize::MAX, &format!("{name} ^{p}"));
            same(&b, 40_000, &format!("{name} ^{p}"));
        }
    }
}

/// Random bytes with a dynamic header: almost all fail in the trees, some decode.
#[test]
fn random_dynamic_streams() {
    let mut r = Rng::new(11);
    for i in 0..1500 {
        let mut b = rand_bytes(i, 1 + r.below(300));
        b[0] = b[0] & !6 | 4;
        same(&b, usize::MAX, &format!("random {i}"));
    }
}

/// As above, but long enough (over 600 bytes) that the header is read from the input buffer with
/// the 64-bit bit buffer (src/header.rs), and real headers with one bit flipped: the errors, their
/// offsets, the output and the read position equal the careful read's.
#[test]
fn fast_header_matches_careful() {
    let mut r = Rng::new(13);
    for i in 0..3000 {
        let mut b = rand_bytes(i + 5000, 700 + r.below(300));
        b[0] = b[0] & !6 | 4;
        same(&b, usize::MAX, &format!("random long {i}"));
    }
    for (name, s) in streams() {
        if s.len() < 700 {
            continue;
        }
        for _ in 0..8 {
            let mut b = s.clone();
            let p = r.below(80 * 8);
            b[p / 8] ^= 1 << (p % 8);
            same(&b, usize::MAX, &format!("{name} header ^{p}"));
        }
    }
}

/// Matches of every length and distance across the margins of the loop.
#[test]
fn matches_across_the_margins() {
    for dist in [1, 2, 3, 5, 7, 8, 9, 15, 16, 17, 255, 4096, 32768] {
        for len in [3, 4, 7, 8, 9, 31, 32, 33, 100, 257, 258] {
            let mut toks = lits(&rand_bytes(dist as u64, dist.min(300)));
            if dist > 300 {
                // History for the distance: a stored block first.
                let s = one(|w| {
                    w.stored(false, &rand_bytes(5, dist))
                        .fixed(true, &[mat(len, dist), lit(b'x'), mat(len, 1)]);
                });
                same(&s, usize::MAX, &format!("dist {dist} len {len}"));
                continue;
            }
            toks.extend([mat(len, dist.min(toks.len())), lit(b'x')]);
            toks.resize(toks.len() + 400, lit(b'y'));
            let s = one(|w| {
                w.fixed(true, &toks);
            });
            same(&s, usize::MAX, &format!("dist {dist} len {len}"));
            same(&s, 50, &format!("dist {dist} len {len}"));
        }
    }
}

/// Codes up to 15 bits: the literals with codes over 11 bits sit in subtables, and a link entry
/// resolves to a literal as well as to a length.
#[test]
fn long_codes_are_literals_too() {
    let mut r = Rng::new(3);
    for round in 0..40 {
        // 13 literals, the end of block and two lengths, in a random order: the first gets a
        // 1-bit code, the next 2 bits, and so on up to two 15-bit codes.
        let mut syms: Vec<usize> = (b'a'..b'a' + 13)
            .map(usize::from)
            .chain([256, 257, 258])
            .collect();
        for i in (1..syms.len()).rev() {
            syms.swap(i, r.below(i + 1));
        }
        let ll = deep_tree(259, &syms);
        let dl = lengths(1, &[(0, 1)]); // distance 1 only
        let mut toks = vec![lit(b'a')];
        for _ in 0..3000 {
            let s = syms[r.below(syms.len())];
            toks.push(match s {
                256 => continue,
                257 => mat(3, 1),
                258 => mat(4, 1),
                s => lit(s as u8),
            });
        }
        let s = one(|w| {
            w.dynamic(true, &ll, &dl, &toks);
        });
        same(&s, usize::MAX, &format!("round {round}"));
        same(&s, 1000, &format!("round {round}"));
        for cut in [s.len() / 2, s.len() - 1] {
            same(&s[..cut], usize::MAX, &format!("round {round} cut {cut}"));
        }
    }
}

/// Dispatch picks the fast tiers: on x86_64 VNNI (AVX2 on a CPU without AVX-512 VNNI) and the AVX2
/// loop, on aarch64 NEON and the generic loop, and scalar and generic under `DEFLATE_SIMD=scalar`.
#[test]
fn dispatch_picks_the_fast_tiers() {
    let scalar = std::env::var_os("DEFLATE_SIMD").is_some_and(|v| v == "scalar");
    #[cfg(target_arch = "x86_64")]
    let vnni = is_x86_feature_detected!("avx512f")
        && is_x86_feature_detected!("avx512bw")
        && is_x86_feature_detected!("avx512vl")
        && is_x86_feature_detected!("avx512vnni");
    #[cfg(not(target_arch = "x86_64"))]
    let vnni = false;
    let want = if scalar {
        [("adler32", "scalar"), ("fast", "generic")]
    } else if cfg!(target_arch = "x86_64") {
        [
            ("adler32", if vnni { "avx512_vnni" } else { "avx2" }),
            ("fast", "avx2"),
        ]
    } else if cfg!(target_arch = "aarch64") {
        [("adler32", "neon"), ("fast", "generic")]
    } else {
        [("adler32", "scalar"), ("fast", "generic")]
    };
    assert_eq!(deflate::tiers(), want);
}
