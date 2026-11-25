//! Hand-built streams: valid ones with the output they must give, invalid ones with the error
//! Go reports (and its offset where it is worked out from the stream), and truncation and limit
//! sweeps of them.

#![allow(clippy::needless_range_loop)]

mod common;
use common::*;
use deflate::Error;

enum Want {
    Out(Vec<u8>),
    Err(Error),
    /// A `CorruptInputError`; the offset is not checked here. The `parity` job runs every case
    /// through Go (`dump_edge_streams`), which checks it.
    Corrupt,
}

struct Case {
    name: String,
    data: Vec<u8>,
    want: Want,
}

fn model(parts: &[&[Tok]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        apply(&mut out, p);
    }
    out
}

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    let mut add = |name: &str, data: Vec<u8>, want: Want| {
        v.push(Case {
            name: name.to_string(),
            data,
            want,
        });
    };
    let no_dist = [0u8];
    // 0..=253 at 8 bits, 254..=257 at 9: literals only.
    let mut lit_tree = vec![8u8; 258];
    lit_tree[254..].fill(9);
    // All 286 litlen codes: 226 at 8 bits, 60 at 9.
    let mut full_tree = vec![9u8; 286];
    full_tree[..226].fill(8);

    add(
        "empty-fixed",
        one(|w| {
            w.fixed(true, &[]);
        }),
        Want::Out(vec![]),
    );
    add(
        "empty-stored",
        one(|w| {
            w.stored(true, &[]);
        }),
        Want::Out(vec![]),
    );
    add(
        "empty-stored-run",
        one(|w| {
            for _ in 0..50 {
                w.stored(false, &[]);
            }
            w.stored(false, b"abc")
                .stored(false, &[])
                .fixed(true, &lits(b"def"));
        }),
        Want::Out(b"abcdef".to_vec()),
    );
    let max = rand_bytes(1, 65535);
    add(
        "stored-max",
        one(|w| {
            w.stored(true, &max);
        }),
        Want::Out(max.clone()),
    );

    // Degenerate and small trees.
    add(
        "degenerate-litlen-eob",
        one(|w| {
            w.dynamic(true, &lengths(257, &[(256, 1)]), &no_dist, &[]);
        }),
        Want::Out(vec![]),
    );
    add(
        "two-code-litlen",
        one(|w| {
            w.dynamic(
                true,
                &lengths(257, &[(65, 1), (256, 1)]),
                &no_dist,
                &lits(&[b'A'; 44]),
            );
        }),
        Want::Out(vec![b'A'; 44]),
    );
    let data = rand_bytes(2, 3000);
    add(
        "empty-dist-literal-only",
        one(|w| {
            w.dynamic(true, &lit_tree, &no_dist, &lits(&data));
        }),
        Want::Out(data.clone()),
    );
    let t = [vec![lit(b'z')], vec![mat(3, 1), mat(258, 1), mat(3, 1)]].concat();
    add(
        "one-dist-code0",
        one(|w| {
            w.dynamic(true, &full_tree, &lengths(1, &[(0, 1)]), &t);
        }),
        Want::Out(model(&[&t])),
    );
    let t = [lits(b"01234567"), vec![mat(10, 8), mat(20, 7)]].concat();
    add(
        "one-dist-code5",
        one(|w| {
            w.dynamic(true, &full_tree, &lengths(6, &[(5, 1)]), &t);
        }),
        Want::Out(model(&[&t])),
    );

    // Deep trees: the end-of-block code is 15 bits, so the first read of a symbol is raised.
    let deep_syms = [
        usize::from(b'a'),
        257,
        usize::from(b'b'),
        258,
        usize::from(b'c'),
        270,
        usize::from(b'd'),
        280,
        usize::from(b'e'),
        284,
        usize::from(b'f'),
        285,
        usize::from(b'g'),
        265,
        usize::from(b'h'),
        256,
    ];
    let deep_ll = deep_tree(286, &deep_syms);
    let deep_dsyms = [0, 29, 1, 15, 2, 20, 3, 25, 4, 10, 5, 6, 7, 8, 9, 11];
    let deep_d = deep_tree(30, &deep_dsyms);
    let mut r = Rng::new(3);
    let mut toks = Vec::new();
    let stored = rand_bytes(3, 32768);
    let mut hist = stored.clone();
    for _ in 0..4000 {
        let s = deep_syms[r.below(deep_syms.len() - 1)];
        if s < 256 {
            toks.push(lit(s as u8));
            continue;
        }
        let c = s - 257;
        let (base, extra) = match c {
            0 => (3, 0),
            1 => (4, 0),
            13 => (23, 2),
            23 => (115, 4),
            27 => (227, 5),
            28 => (258, 0),
            8 => (11, 1),
            _ => unreachable!(),
        };
        let l = base + r.below(1 << extra);
        let dc = deep_dsyms[r.below(10)];
        let d = DIST_BASE[dc] + r.below(1 << dist_extra(dc));
        toks.push(mat(l, d));
    }
    apply(&mut hist, &toks);
    add(
        "deep-trees",
        one(|w| {
            w.stored(false, &stored)
                .dynamic(true, &deep_ll, &deep_d, &toks);
        }),
        Want::Out(hist),
    );
    add(
        "deep-eob-only",
        one(|w| {
            w.dynamic(true, &deep_ll, &deep_d, &[]);
        }),
        Want::Out(vec![]),
    );

    // Lengths and distances.
    let t = [lit(b'x'), mat(258, 1), mat(258, 1), mat(258, 1)];
    add(
        "len258-code285",
        one(|w| {
            w.fixed(true, &t);
        }),
        Want::Out(model(&[&t])),
    );
    // Length 258 as code 284 with 5 extra bits of 31: Go accepts it.
    let t284 = Tok {
        sym: 284,
        ext: 31,
        next: 5,
        d: Some((0, 0, 0)),
        len: 258,
        dist: 1,
    };
    let t = [lit(b'x'), t284, t284, mat(3, 1)];
    add(
        "len258-code284",
        one(|w| {
            w.fixed(true, &t);
        }),
        Want::Out(model(&[&t])),
    );
    let mut t = lits(&rand_bytes(4, 300));
    for c in 0..29 {
        let (b, e) = (LEN_BASE[c], (1usize << LEN_EXTRA[c]) - 1);
        t.push(mat(b, 1 + c));
        t.push(mat(b + e, 300 - c));
    }
    add(
        "all-length-codes",
        one(|w| {
            w.fixed(true, &t);
        }),
        Want::Out(model(&[&t])),
    );
    let mut t = Vec::new();
    for c in 0..30 {
        t.push(mat(3 + c, DIST_BASE[c]));
        t.push(mat(258, DIST_BASE[c] + (1 << dist_extra(c)) - 1));
    }
    let pre = rand_bytes(5, 32768);
    let mut hist = pre.clone();
    apply(&mut hist, &t);
    add(
        "all-dist-codes",
        one(|w| {
            w.stored(false, &pre).fixed(true, &t);
        }),
        Want::Out(hist),
    );
    let mut dl = vec![5u8; 30]; // 28 at 5 bits and 2 at 4 is complete
    dl[..2].fill(4);
    let mut hist = pre.clone();
    apply(&mut hist, &t);
    add(
        "all-dist-codes-dynamic-rle",
        one(|w| {
            w.stored(false, &pre).dynamic_rle(true, &full_tree, &dl, &t);
        }),
        Want::Out(hist),
    );
    let t = [mat(258, 32768), mat(258, 32768), lit(b'e'), mat(3, 32768)];
    let mut hist = pre.clone();
    apply(&mut hist, &t);
    add(
        "dist-32768",
        one(|w| {
            w.stored(false, &pre).fixed(true, &t);
        }),
        Want::Out(hist),
    );
    let mut t = lits(&rand_bytes(10, 64));
    for d in 1..=64 {
        for l in 3..=258 {
            t.push(mat(l, d));
        }
        t.push(lit(d as u8));
    }
    add(
        "overlap-dist1-64",
        one(|w| {
            w.fixed(true, &t);
        }),
        Want::Out(model(&[&t])),
    );

    // Header codes 16, 17, 18, also across the litlen/dist boundary.
    let mut ll = vec![0u8; 286];
    ll[..258].copy_from_slice(&lit_tree);
    let t = [lits(b"rle header"), vec![mat(3, 1)]].concat();
    add(
        "dynamic-rle",
        one(|w| {
            w.dynamic_rle(true, &ll, &lengths(30, &[(0, 1), (29, 1)]), &t);
        }),
        Want::Out(model(&[&t])),
    );
    let mut ll = vec![0u8; 260];
    ll[..258].copy_from_slice(&lit_tree);
    add(
        "repeat-crosses-boundary",
        one(|w| {
            let mut syms = rle_syms(&ll[..257]);
            syms.push(ClSym {
                sym: 9,
                ext: 0,
                n: 0,
            });
            syms.push(ClSym {
                sym: 17,
                ext: 2,
                n: 3,
            });
            w.dyn_header(true, (260, 3, 19), &rle_cl(), &syms);
            w.body(&ll, &[0, 0, 0], &lits(b"crossing"));
        }),
        Want::Out(b"crossing".to_vec()),
    );

    // Many blocks, and every bit alignment before each kind of block.
    let mut hist = Vec::new();
    let s = one(|w| {
        for i in 0..1000usize {
            let b = i as u8;
            hist.push(b);
            match i % 3 {
                0 => {
                    w.fixed(false, &[lit(b)]);
                }
                1 => {
                    w.stored(false, &[b]);
                }
                _ => {
                    w.dynamic(
                        false,
                        &lengths(257, &[(i % 256, 1), (256, 1)]),
                        &no_dist,
                        &[lit(b)],
                    );
                }
            }
        }
        w.fixed(true, &[]);
    });
    add("many-tiny-blocks", s, Want::Out(hist));
    for m in 0..8u8 {
        let nine: Vec<Tok> = (0..m).map(|i| lit(200 + i)).collect();
        for next in ["stored", "fixed", "dynamic"] {
            let t = [lits(b"fixed"), vec![mat(4, 3)]].concat();
            let t2 = [lits(b"dyn"), vec![mat(5, 1)]].concat();
            let mut want = model(&[&nine]);
            let s = one(|w| {
                w.fixed(false, &nine);
                match next {
                    "stored" => {
                        w.stored(false, b"stored");
                        want.extend_from_slice(b"stored");
                    }
                    "fixed" => {
                        w.fixed(false, &t);
                        apply(&mut want, &t);
                    }
                    _ => {
                        w.dynamic(false, &full_tree, &lengths(1, &[(0, 1)]), &t2);
                        apply(&mut want, &t2);
                    }
                }
                w.fixed(true, &nine);
            });
            apply(&mut want, &nine);
            add(&format!("align{}-{next}", (2 + m) % 8), s, Want::Out(want));
        }
    }

    // Errors with an offset worked out by hand: the bytes Go has read when it fails.
    add(
        "reserved-btype3",
        one(|w| {
            w.header(true, 3);
        }),
        Want::Err(Error::Corrupt(1)),
    );
    add(
        "reserved-btype3-after-block",
        one(|w| {
            w.fixed(false, &lits(b"hello")).header(true, 3);
        }),
        Want::Err(Error::Corrupt(7)),
    );
    add(
        "stored-nlen-mismatch",
        one(|w| {
            w.fixed(false, &[lit(b'x')])
                .stored_raw(true, 5, 0xfffe, b"abcde");
        }),
        Want::Err(Error::Corrupt(7)),
    );
    add(
        "stored-nlen-zero-mismatch",
        one(|w| {
            w.stored_raw(true, 0, 0, &[]);
        }),
        Want::Err(Error::Corrupt(5)),
    );
    add(
        "stored-truncated-len",
        one(|w| {
            w.fixed(false, &[lit(b'x')]).header(true, 0);
            w.pad();
            w.bits(5, 8);
        }),
        Want::Err(Error::UnexpectedEof),
    );
    add(
        "dist-at-start",
        one(|w| {
            w.fixed(true, &[mat(3, 1)]);
        }),
        Want::Err(Error::Corrupt(2)),
    );
    add(
        "dist-beyond-history",
        one(|w| {
            w.fixed(true, &[lit(b'a'), mat(3, 2)]);
        }),
        Want::Err(Error::Corrupt(3)),
    );
    for s in [286, 287] {
        let t = [lits(b"abcdef"), vec![sym(s)]].concat();
        add(
            &format!("fixed-litlen-{s}"),
            one(|w| {
                w.fixed(true, &t);
            }),
            Want::Err(Error::Corrupt(8)),
        );
    }
    for dc in [30, 31] {
        let mut t = sym(257);
        t.d = Some((dc, 0, 0));
        let t = [lits(b"abcdef"), vec![t]].concat();
        add(
            &format!("fixed-dist-{dc}"),
            one(|w| {
                w.fixed(true, &t);
            }),
            Want::Err(Error::Corrupt(8)),
        );
    }
    for nlit in [287usize, 288] {
        add(
            &format!("dynamic-hlit-{nlit}"),
            one(|w| {
                w.header(true, 2);
                w.bits((nlit - 257) as u64, 5);
                w.bits(0, 9);
            }),
            Want::Err(Error::Corrupt(3)),
        );
    }
    for ndist in [31u64, 32] {
        add(
            &format!("dynamic-hdist-{ndist}"),
            one(|w| {
                w.header(true, 2);
                w.bits(0, 5);
                w.bits(ndist - 1, 5);
                w.bits(0, 4);
            }),
            Want::Err(Error::Corrupt(3)),
        );
    }
    add(
        "no-final-block",
        one(|w| {
            w.fixed(false, &lits(b"abc"));
        }),
        Want::Err(Error::UnexpectedEof),
    );
    add(
        "no-eob-code",
        one(|w| {
            w.dynamic(
                true,
                &lengths(258, &[(97, 1), (98, 1)]),
                &no_dist,
                &lits(b"abba"),
            );
        }),
        Want::Err(Error::UnexpectedEof),
    );

    // Bad trees and distances that Go reports as corrupt.
    add(
        "degenerate-litlen-bad-code",
        one(|w| {
            w.dynamic(false, &lengths(257, &[(256, 1)]), &no_dist, &[]);
            // The end-of-block code is written after the tokens, so this bit starts a next block.
            w.bits(1, 1);
        }),
        Want::Err(Error::UnexpectedEof),
    );
    add(
        "one-dist-bad-code",
        one(|w| {
            // Distance bit 1 is the unassigned code: no code is written, only the extra bit.
            let bad = Tok {
                d: Some((99, 1, 1)),
                ..sym(257)
            };
            w.dynamic(true, &full_tree, &lengths(1, &[(0, 1)]), &[lit(b'q'), bad]);
        }),
        Want::Corrupt,
    );
    let mut cl = [0u8; 19];
    cl[8] = 2;
    cl[9] = 2;
    add(
        "clen-incomplete",
        one(|w| {
            w.dyn_header(true, (257, 1, 19), &cl, &[]);
        }),
        Want::Corrupt,
    );
    let mut cl = [0u8; 19];
    cl[8] = 1;
    let eight = ClSym {
        sym: 8,
        ext: 0,
        n: 0,
    };
    add(
        "clen-degenerate",
        one(|w| {
            w.dyn_header(true, (257, 1, 19), &cl, &[eight; 258]);
        }),
        Want::Corrupt,
    );
    let mut cl = [0u8; 19];
    cl[18] = 1;
    cl[0] = 1;
    let run = |ext| ClSym { sym: 18, ext, n: 7 };
    add(
        "clen-hclen4",
        one(|w| {
            w.dyn_header(true, (257, 1, 4), &cl, &[run(127), run(108)]);
        }),
        Want::Corrupt,
    );
    add(
        "dist-32768-short-history",
        one(|w| {
            w.stored(false, &rand_bytes(8, 32767))
                .fixed(true, &[mat(3, 32768)]);
        }),
        Want::Corrupt,
    );
    add(
        "dist-beyond-after-32k",
        one(|w| {
            w.stored(false, &rand_bytes(9, 30000))
                .fixed(true, &[lits(b"abc"), vec![mat(3, 30004)]].concat());
        }),
        Want::Corrupt,
    );
    let mut t = lits(&rand_bytes(11, 64));
    for d in 1..=64 {
        for l in [3, 4, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 257, 258] {
            t.push(mat(l, d));
            t.push(lit(l as u8));
        }
    }
    add(
        "overlap-dist1-64-short",
        one(|w| {
            w.fixed(true, &t);
        }),
        Want::Out(model(&[&t])),
    );

    // Corrupt, offset not worked out.
    add(
        "litlen-oversubscribed",
        one(|w| {
            w.dynamic(
                true,
                &lengths(257, &[(97, 1), (98, 1), (256, 1)]),
                &no_dist,
                &[],
            );
        }),
        Want::Corrupt,
    );
    add(
        "litlen-incomplete",
        one(|w| {
            w.dynamic(
                true,
                &lengths(257, &[(97, 2), (98, 2), (256, 2)]),
                &no_dist,
                &[],
            );
        }),
        Want::Corrupt,
    );
    add(
        "dist-oversubscribed",
        one(|w| {
            w.dynamic(true, &lit_tree, &lengths(3, &[(0, 1), (1, 1), (2, 1)]), &[]);
        }),
        Want::Corrupt,
    );
    add(
        "dist-incomplete",
        one(|w| {
            w.dynamic(true, &lit_tree, &lengths(3, &[(0, 2), (1, 2)]), &[]);
        }),
        Want::Corrupt,
    );
    add(
        "empty-dist-used",
        one(|w| {
            w.dynamic(
                true,
                &lit_tree,
                &no_dist,
                &[lits(b"abcd"), vec![mat(3, 1)]].concat(),
            );
        }),
        Want::Corrupt,
    );
    add(
        "clen-oversubscribed",
        one(|w| {
            let mut cl = [0u8; 19];
            cl[8] = 1;
            cl[9] = 1;
            cl[0] = 1;
            w.dyn_header(true, (257, 1, 19), &cl, &[]);
        }),
        Want::Corrupt,
    );
    add(
        "repeat16-first",
        one(|w| {
            w.dyn_header(
                true,
                (257, 1, 19),
                &rle_cl(),
                &[ClSym {
                    sym: 16,
                    ext: 0,
                    n: 2,
                }],
            );
        }),
        Want::Corrupt,
    );
    for s in [(16, 3, 2), (17, 7, 3), (18, 127, 7)] {
        add(
            &format!("repeat{}-overflow", s.0),
            one(|w| {
                let mut syms = plain_syms(&lit_tree[..256]);
                syms.push(ClSym {
                    sym: 9,
                    ext: 0,
                    n: 0,
                });
                syms.push(ClSym {
                    sym: 9,
                    ext: 0,
                    n: 0,
                });
                // 258 litlen and 1 distance length are wanted; the repeat runs past them.
                syms.push(ClSym {
                    sym: s.0,
                    ext: s.1,
                    n: s.2,
                });
                w.dyn_header(true, (258, 1, 19), &rle_cl(), &syms);
            }),
            Want::Corrupt,
        );
    }
    for eob_at in EOB_AT {
        let (s, out) = eob_long(eob_at, false);
        add(&format!("eob-long{eob_at}"), s, Want::Out(out));
    }
    // Go exactCapRepros: a stored block cut short.
    add(
        "exactcap-stored-ffff",
        vec![0x30, 0xff, 0xff, 0x00, 0x00, 0x30],
        Want::Err(Error::UnexpectedEof),
    );
    add(
        "exactcap-stored-2000",
        vec![0x30, 0x00, 0x20, 0xff, 0xdf, 0x30],
        Want::Err(Error::UnexpectedEof),
    );
    v
}

/// Go TestAuditEOBLong: trees whose end-of-block code is long (the symbol at `eob_at` of the 16
/// that get lengths 1..15, 15 swaps places with 256), three dynamic blocks with a stored and a
/// fixed one between. Returns the stream and what it decodes to.
const EOB_AT: [usize; 5] = [15, 13, 9, 4, 1];

fn eob_long(eob_at: usize, trailing: bool) -> (Vec<u8>, Vec<u8>) {
    let mut syms = [
        97, 257, 98, 265, 99, 100, 270, 101, 102, 280, 103, 284, 104, 105, 122, 256,
    ];
    syms.swap(eob_at, 15);
    let ll = deep_tree(286, &syms);
    let dl = lengths(30, &[(0, 2), (1, 2), (6, 2), (15, 2)]);
    let lit_syms: Vec<usize> = syms.iter().copied().filter(|&s| s < 256).collect();
    let len_syms: Vec<usize> = syms.iter().copied().filter(|&s| s > 256).collect();
    let mut r = Rng::new(eob_at as u64);
    let mut produced = 0;
    let mut toks = |n: usize| {
        let mut t = Vec::new();
        while t.len() < n {
            if produced < 300 || r.below(5) < 3 {
                // Skew to the shortest literal.
                let c = lit_syms[r.below(lit_syms.len()).min(r.below(3))];
                t.push(lit(c as u8));
                produced += 1;
                continue;
            }
            let ls = len_syms[r.below(len_syms.len())];
            let c = ls - 257;
            let x = r.next_u64() & ((1 << LEN_EXTRA[c]) - 1);
            let dc = [0, 1, 6, 15][r.below(4)];
            let y = r.next_u64() & ((1 << dist_extra(dc)) - 1);
            let (len, dist) = (LEN_BASE[c] + x as usize, DIST_BASE[dc] + y as usize);
            t.push(Tok {
                sym: ls,
                ext: x,
                next: LEN_EXTRA[c],
                d: Some((dc, y, dist_extra(dc))),
                len,
                dist,
            });
            produced += len;
        }
        t
    };
    let (t1, t2, t3) = (toks(900), toks(700), toks(1200));
    let stored = rand_bytes(eob_at as u64, 40);
    let fixed = lits(b"fixed between dynamic");
    let mut s = one(|w| {
        w.dynamic(false, &ll, &dl, &t1)
            .stored(false, &stored)
            .dynamic(false, &ll, &dl, &t2)
            .fixed(false, &fixed)
            .dynamic(true, &ll, &dl, &t3);
    });
    let mut out = Vec::new();
    apply(&mut out, &t1);
    out.extend_from_slice(&stored);
    for t in [&t2[..], &fixed[..], &t3[..]] {
        apply(&mut out, t);
    }
    if trailing {
        s.extend_from_slice(b"trailing bytes after the final block, more than the margin");
    }
    (s, out)
}

fn check(name: &str, got: &Result<Vec<u8>, Error>, want: &Want) {
    match (got, want) {
        (Ok(g), Want::Out(w)) => assert!(
            g == w,
            "{name}: output differs (len {} want {})",
            g.len(),
            w.len()
        ),
        (Err(g), Want::Err(w)) => assert_eq!(g, w, "{name}"),
        (Err(Error::Corrupt(_)), Want::Corrupt) => {}
        (g, _) => panic!("{name}: got {:?}", g.as_ref().map(Vec::len)),
    }
}

/// For the CI `parity` job: with `DEFLATE_EDGE_DUMP=<dir>`, writes every case as
/// `<dir>/edge-<name>.raw`, so Go decodes them too and supplies the exact error offsets.
#[test]
fn dump_edge_streams() {
    let Some(dir) = std::env::var_os("DEFLATE_EDGE_DUMP") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for c in cases() {
        let path = std::path::Path::new(&dir).join(format!("edge-{}.raw", c.name));
        std::fs::write(path, &c.data).unwrap();
    }
}

#[test]
fn edge_streams_decode() {
    for c in cases() {
        check(&c.name, &inflate(&c.data), &c.want);
    }
}

#[test]
fn edge_streams_reader_equals_decode() {
    for c in cases() {
        let want = inflate(&c.data);
        for (cap, bufsz) in [(0, 4096), (0, 1), (1, 7), (16, 33000)] {
            assert_eq!(
                read_raw(&c.data, &[], cap, bufsz),
                want,
                "{} cap {cap} buf {bufsz}",
                c.name
            );
        }
    }
}

#[test]
fn truncated_valid_streams_are_unexpected_eof() {
    for c in cases() {
        if !matches!(c.want, Want::Out(_)) || c.data.len() > 8192 {
            continue;
        }
        for i in 0..c.data.len() {
            assert_eq!(
                inflate(&c.data[..i]),
                Err(Error::UnexpectedEof),
                "{} truncated at {i}",
                c.name
            );
        }
    }
}

/// Go TestAuditEOBLong's `trailing` streams: bytes after the final block change nothing.
#[test]
fn eob_long_trailing_bytes_are_ignored() {
    for eob_at in EOB_AT {
        let (s, out) = eob_long(eob_at, true);
        assert_eq!(inflate(&s).as_ref(), Ok(&out), "eob_at {eob_at}");
        for (cap, bufsz) in [(0, 4096), (0, 1), (16, 33000)] {
            assert_eq!(read_raw(&s, &[], cap, bufsz).as_ref(), Ok(&out));
        }
    }
}

#[test]
fn trailing_bytes_are_ignored() {
    let mut s = one(|w| {
        w.fixed(true, &lits(b"abc"));
    });
    s.extend_from_slice(b"garbage after end");
    assert_eq!(inflate(&s).unwrap(), b"abc");
}

#[test]
fn decode_limit_stops_at_n() {
    for c in cases() {
        let Want::Out(full) = &c.want else { continue };
        let l = full.len();
        for n in [
            0,
            1,
            2,
            100,
            32767,
            32768,
            32769,
            l.saturating_sub(1),
            l,
            l + 1,
            l + 1000,
        ] {
            let want = full[..n.min(l)].to_vec();
            assert_eq!(inflate_limit(&c.data, n), Ok(want), "{} n={n}", c.name);
        }
    }
}

/// Streams that stop where stdlib's `LimitReader` over `flate.NewReader` stops, from the Go
/// TestLimitStreams table.
#[test]
fn decode_limit_matches_limit_reader() {
    let abc = lits(b"abc");
    let reserved = one(|w| {
        w.fixed(false, &abc).header(true, 3);
    });
    assert_eq!(inflate_limit(&reserved, 2), Ok(b"ab".to_vec()));
    assert!(matches!(
        inflate_limit(&reserved, 3),
        Err(Error::Corrupt(_))
    ));

    let stored_empty = one(|w| {
        w.fixed(false, &abc).stored(false, &[]).header(true, 3);
    });
    assert_eq!(inflate_limit(&stored_empty, 3), Ok(b"abc".to_vec()));

    let truncated = one(|w| {
        w.fixed(false, &abc);
    });
    assert_eq!(inflate_limit(&truncated, 3), Err(Error::UnexpectedEof));

    let bad_dist = one(|w| {
        w.fixed(true, &[abc.clone(), vec![mat(3, 4)]].concat());
    });
    assert!(matches!(
        inflate_limit(&bad_dist, 3),
        Err(Error::Corrupt(_))
    ));

    // A window flush at a multiple of 32 KiB ends the read, whatever follows.
    let win = lits(&rand_bytes(6, 32768));
    let w_reserved = one(|w| {
        w.fixed(false, &win).header(true, 3);
    });
    assert_eq!(
        inflate_limit(&w_reserved, 32768).map(|v| v.len()),
        Ok(32768)
    );
    assert!(matches!(
        inflate_limit(&w_reserved, 32769),
        Err(Error::Corrupt(_))
    ));
    let w_trunc = one(|w| {
        w.fixed(false, &win);
    });
    assert_eq!(inflate_limit(&w_trunc, 32768).map(|v| v.len()), Ok(32768));
    assert_eq!(inflate_limit(&w_trunc, 32769), Err(Error::UnexpectedEof));

    // A sync flush (empty stored block) at 1000 bytes, then nothing or garbage.
    let text = rand_bytes(7, 1000);
    let flushed = one(|w| {
        w.fixed(false, &lits(&text)).stored(false, &[]);
    });
    assert_eq!(inflate_limit(&flushed, 1000).map(|v| v.len()), Ok(1000));
    let mut garbage = flushed.clone();
    garbage.extend_from_slice(&[0xff, 0xff, 0xff]);
    assert_eq!(inflate_limit(&garbage, 999).map(|v| v.len()), Ok(999));
    assert!(matches!(
        inflate_limit(&garbage, 1001),
        Err(Error::Corrupt(_))
    ));
}
