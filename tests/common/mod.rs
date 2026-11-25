//! Shared test helpers: a DEFLATE bit writer for hand-built streams, a model of what a stream
//! decodes to, a deterministic generator and the fixture loader.
#![allow(dead_code, clippy::needless_range_loop)]

use deflate::{Error, Limits};
use std::io::{self, Read};
use std::sync::atomic::AtomicBool;

pub static NO_CANCEL: AtomicBool = AtomicBool::new(false);

pub fn inflate(src: &[u8]) -> Result<Vec<u8>, Error> {
    deflate::decode(src, &Limits::default(), &NO_CANCEL)
}

pub fn inflate_limit(src: &[u8], n: usize) -> Result<Vec<u8>, Error> {
    deflate::decode_limit(src, n, &Limits::default(), &NO_CANCEL)
}

pub fn unzlib(src: &[u8]) -> Result<Vec<u8>, Error> {
    deflate::zlib::decode(src, &Limits::default(), &NO_CANCEL)
}

/// The [`Error`] inside an `io::Error` that a reader returned.
pub fn into_error(e: io::Error) -> Error {
    e.get_ref()
        .and_then(|s| s.downcast_ref::<Error>())
        .cloned()
        .unwrap_or_else(|| Error::Io(e.kind()))
}

/// Reads `r` to the end with a `bufsz` buffer: the bytes, or the first error.
pub fn read_all(mut r: impl Read, bufsz: usize) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; bufsz];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return Err(into_error(e)),
        }
    }
}

/// Raw Reader over `src`, with a plain slice (`cap == 0`) or a `BufReader` of `cap` bytes.
pub fn read_raw(src: &[u8], dict: &[u8], cap: usize, bufsz: usize) -> Result<Vec<u8>, Error> {
    let l = Limits::default();
    if cap == 0 {
        read_all(deflate::Reader::with_dict(src, dict, &l, &NO_CANCEL), bufsz)
    } else {
        let br = io::BufReader::with_capacity(cap, src);
        read_all(deflate::Reader::with_dict(br, dict, &l, &NO_CANCEL), bufsz)
    }
}

pub fn read_zlib(src: &[u8], dict: &[u8], cap: usize, bufsz: usize) -> Result<Vec<u8>, Error> {
    let l = Limits::default();
    if cap == 0 {
        read_all(
            deflate::zlib::Reader::with_dict(src, dict, &l, &NO_CANCEL)?,
            bufsz,
        )
    } else {
        let br = io::BufReader::with_capacity(cap, src);
        read_all(
            deflate::zlib::Reader::with_dict(br, dict, &l, &NO_CANCEL)?,
            bufsz,
        )
    }
}

/// xorshift64*: deterministic bytes and picks, no dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() >> 11) as usize % n
    }
}

pub fn rand_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut r = Rng::new(seed);
    (0..n).map(|_| (r.next_u64() >> 24) as u8).collect()
}

/// Writes DEFLATE bits, LSB first.
#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

pub const LEN_BASE: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
pub const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
pub const DIST_BASE: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
pub const CODE_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

pub fn dist_extra(c: usize) -> u32 {
    if c < 4 { 0 } else { (c as u32 - 2) / 2 }
}

/// One litlen symbol with its extra bits, plus an optional distance symbol (symbol, extra bits
/// value, number of extra bits). `len` and `dist` are what a match means; `len == 0` is a
/// literal or a bare symbol.
#[derive(Clone, Copy)]
pub struct Tok {
    pub sym: usize,
    pub ext: u64,
    pub next: u32,
    pub d: Option<(usize, u64, u32)>,
    pub len: usize,
    pub dist: usize,
}

pub fn lit(b: u8) -> Tok {
    sym(usize::from(b))
}

pub fn sym(s: usize) -> Tok {
    Tok {
        sym: s,
        ext: 0,
        next: 0,
        d: None,
        len: 0,
        dist: 0,
    }
}

pub fn lits(b: &[u8]) -> Vec<Tok> {
    b.iter().map(|&c| lit(c)).collect()
}

/// A match with the usual codes.
pub fn mat(len: usize, dist: usize) -> Tok {
    let c = (0..29)
        .rev()
        .find(|&c| len >= LEN_BASE[c] && len - LEN_BASE[c] < 1 << LEN_EXTRA[c])
        .unwrap();
    let dc = (0..30).rev().find(|&c| dist >= DIST_BASE[c]).unwrap();
    Tok {
        sym: 257 + c,
        ext: (len - LEN_BASE[c]) as u64,
        next: LEN_EXTRA[c],
        d: Some((dc, (dist - DIST_BASE[dc]) as u64, dist_extra(dc))),
        len,
        dist,
    }
}

/// Appends what `toks` decode to onto `out`, which holds the history.
pub fn apply(out: &mut Vec<u8>, toks: &[Tok]) {
    for t in toks {
        if t.len > 0 {
            for _ in 0..t.len {
                out.push(out[out.len() - t.dist]);
            }
        } else if t.sym < 256 {
            out.push(t.sym as u8);
        }
    }
}

/// Canonical Huffman codes (RFC 1951 section 3.2.2); bad trees get junk codes.
pub fn canonical(lengths: &[u8]) -> Vec<u32> {
    let mut count = [0u32; 16];
    for &n in lengths {
        count[usize::from(n)] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0;
    for i in 1..16 {
        code = (code + count[i - 1]) << 1;
        next[i] = code;
    }
    lengths
        .iter()
        .map(|&n| {
            let n = usize::from(n);
            let c = next[n];
            if n > 0 {
                next[n] += 1;
            }
            c
        })
        .collect()
}

pub fn fixed_ll() -> Vec<u8> {
    let mut l = vec![8u8; 288];
    l[144..256].fill(9);
    l[256..280].fill(7);
    l
}

pub fn lengths(n: usize, pairs: &[(usize, u8)]) -> Vec<u8> {
    let mut l = vec![0u8; n];
    for &(i, v) in pairs {
        l[i] = v;
    }
    l
}

/// Code lengths that give every symbol of `syms` a length 1..14, then 15, 15: a complete tree.
pub fn deep_tree(n: usize, syms: &[usize]) -> Vec<u8> {
    let mut l = vec![0u8; n];
    for (i, &s) in syms.iter().enumerate() {
        l[s] = (i + 1).min(15) as u8;
    }
    l
}

/// A code length symbol with its extra bits.
#[derive(Clone, Copy)]
pub struct ClSym {
    pub sym: usize,
    pub ext: u64,
    pub n: u32,
}

/// Code length code that gives 0..15 four bits each.
pub fn plain_cl() -> [u8; 19] {
    let mut c = [0u8; 19];
    c[..16].fill(4);
    c
}

/// Code length code for 16, 17, 18 too: 13 symbols of 4 bits, 6 of 5.
pub fn rle_cl() -> [u8; 19] {
    let mut c = [4u8; 19];
    c[13..].fill(5);
    c
}

pub fn plain_syms(l: &[u8]) -> Vec<ClSym> {
    l.iter()
        .map(|&n| ClSym {
            sym: usize::from(n),
            ext: 0,
            n: 0,
        })
        .collect()
}

/// Uses 16, 17, 18 runs, crossing the litlen/dist boundary.
pub fn rle_syms(l: &[u8]) -> Vec<ClSym> {
    let mut s = Vec::new();
    let plain = |v: u8| ClSym {
        sym: usize::from(v),
        ext: 0,
        n: 0,
    };
    let mut i = 0;
    while i < l.len() {
        let v = l[i];
        let mut j = i;
        while j < l.len() && l[j] == v {
            j += 1;
        }
        let mut run = j - i;
        i = j;
        if v == 0 {
            while run >= 11 {
                let k = run.min(138);
                s.push(ClSym {
                    sym: 18,
                    ext: (k - 11) as u64,
                    n: 7,
                });
                run -= k;
            }
            if run >= 3 {
                s.push(ClSym {
                    sym: 17,
                    ext: (run - 3) as u64,
                    n: 3,
                });
                run = 0;
            }
        } else {
            s.push(plain(v));
            run -= 1;
            while run >= 3 {
                let k = run.min(6);
                s.push(ClSym {
                    sym: 16,
                    ext: (k - 3) as u64,
                    n: 2,
                });
                run -= k;
            }
        }
        for _ in 0..run {
            s.push(plain(v));
        }
    }
    s
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bits(&mut self, v: u64, n: u32) {
        self.acc |= (v & ((1u64 << n) - 1)) << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// A Huffman code, which is MSB first.
    pub fn code(&mut self, c: u32, n: u32) {
        if n > 0 {
            self.bits(u64::from(c.reverse_bits() >> (32 - n)), n);
        }
    }

    pub fn pad(&mut self) {
        if self.n > 0 {
            self.bits(0, 8 - self.n);
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.pad();
        self.out
    }

    pub fn header(&mut self, fin: bool, typ: u64) -> &mut Self {
        self.bits(u64::from(fin), 1);
        self.bits(typ, 2);
        self
    }

    pub fn stored(&mut self, fin: bool, data: &[u8]) -> &mut Self {
        let n = data.len();
        self.stored_raw(fin, n, !n & 0xffff, data)
    }

    pub fn stored_raw(&mut self, fin: bool, n: usize, nn: usize, data: &[u8]) -> &mut Self {
        self.header(fin, 0);
        self.pad();
        self.bits(n as u64, 16);
        self.bits(nn as u64, 16);
        for &b in data {
            self.bits(u64::from(b), 8);
        }
        self
    }

    /// `toks`, then end of block, with the given codes.
    pub fn body(&mut self, ll: &[u8], dl: &[u8], toks: &[Tok]) -> &mut Self {
        let (llc, dc) = (canonical(ll), canonical(dl));
        for t in toks.iter().copied().chain([sym(256)]) {
            if t.sym < ll.len() {
                self.code(llc[t.sym], u32::from(ll[t.sym]));
            }
            self.bits(t.ext, t.next);
            if let Some((ds, de, dn)) = t.d {
                if ds < dl.len() {
                    self.code(dc[ds], u32::from(dl[ds]));
                }
                self.bits(de, dn);
            }
        }
        self
    }

    pub fn fixed(&mut self, fin: bool, toks: &[Tok]) -> &mut Self {
        self.header(fin, 1);
        self.body(&fixed_ll(), &[5u8; 32], toks)
    }

    pub fn dyn_header(
        &mut self,
        fin: bool,
        (nlit, ndist, nclen): (usize, usize, usize),
        cl: &[u8; 19],
        syms: &[ClSym],
    ) -> &mut Self {
        self.header(fin, 2);
        self.bits((nlit - 257) as u64, 5);
        self.bits((ndist - 1) as u64, 5);
        self.bits((nclen - 4) as u64, 4);
        for &o in &CODE_ORDER[..nclen] {
            self.bits(u64::from(cl[o]), 3);
        }
        let codes = canonical(cl);
        for s in syms {
            self.code(codes[s.sym], u32::from(cl[s.sym]));
            self.bits(s.ext, s.n);
        }
        self
    }

    /// A dynamic block, code lengths written one symbol each.
    pub fn dynamic(&mut self, fin: bool, ll: &[u8], dl: &[u8], toks: &[Tok]) -> &mut Self {
        let all = [ll, dl].concat();
        self.dyn_header(
            fin,
            (ll.len(), dl.len(), 19),
            &plain_cl(),
            &plain_syms(&all),
        );
        self.body(ll, dl, toks)
    }

    /// A dynamic block, code lengths written with 16, 17 and 18.
    pub fn dynamic_rle(&mut self, fin: bool, ll: &[u8], dl: &[u8], toks: &[Tok]) -> &mut Self {
        let all = [ll, dl].concat();
        self.dyn_header(fin, (ll.len(), dl.len(), 19), &rle_cl(), &rle_syms(&all));
        self.body(ll, dl, toks)
    }
}

pub fn one(f: impl FnOnce(&mut BitWriter)) -> Vec<u8> {
    let mut w = BitWriter::new();
    f(&mut w);
    w.finish()
}

/// One row of `tests/data/fixtures/MANIFEST.tsv`.
pub struct Fixture {
    pub file: String,
    pub input: String,
    pub insize: usize,
    /// The preset dictionary, empty if none.
    pub dict: Vec<u8>,
    pub data: Vec<u8>,
}

pub fn fixtures() -> Vec<Fixture> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fixtures");
    let manifest = std::fs::read_to_string(format!("{dir}/MANIFEST.tsv")).unwrap();
    manifest
        .lines()
        .skip(2)
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let dict = if f[6] == "-" {
                Vec::new()
            } else {
                let stem = f[0].split('.').next().unwrap();
                std::fs::read(format!("{dir}/{stem}.dict")).unwrap()
            };
            Fixture {
                file: f[0].to_string(),
                input: f[7].to_string(),
                insize: f[8].parse().unwrap(),
                dict,
                data: std::fs::read(format!("{dir}/{}", f[0])).unwrap(),
            }
        })
        .collect()
}

/// The raw DEFLATE body of a zlib stream.
pub fn zlib_body(z: &[u8]) -> &[u8] {
    &z[if z[1] & 0x20 != 0 { 6 } else { 2 }..]
}
