//! The fast decode loop: a port of the Go loop of deflate-go `internal/inflate` (`fast_generic.go`
//! and `tables.go`, see NOTICE). The tables are here; the loop, which writes the output through a
//! raw pointer, is in `fast/kernel.rs`.
//!
//! The loop decodes the common symbols of a Huffman block between symbols of the careful path
//! (`inflate.rs`), which owns every error and every read decision. It returns before a symbol,
//! with no side effect for it, when the symbol is anything it does not handle (end of block, a
//! bad code, a distance past the history, a missing code of an incomplete tree) or when it is
//! near the end of the input or the output. The careful path then decodes that one symbol and
//! calls the loop again. The loop hands back the state stdlib's reader would have: the bytes it
//! pulled from the input, and the bits left over (`Slack`, below).
//!
//! Both tables are indexed by the low bits of the bit buffer: `LL_BITS` bits for litlen,
//! `DIST_BITS` for distance. A code longer than that goes through a link entry to a subtable,
//! indexed by the next bits. An entry is a `u32`:
//!
//! ```text
//!  bits  0-4   bits to consume: code length + extra bits
//!  bit   5     LINK: bits 16-31 are the subtable start, bits 8-11 its index width
//!  bit   6     STOP: the careful path decodes this symbol
//!  bit   7     LIT: bits 16-23 are a literal byte (litlen only)
//!  bits  8-11  code length n
//!  bits 12-15  litlen only: max(min length, n), the bits stdlib reads for the symbol
//!  bits 16-31  length base, distance base, or subtable start
//!  bits 24-27  literal only: that minus n
//! ```
//!
//! The extra bits of a length or distance are `(bitbuf & (1 << consume) - 1) >> n`.
//!
//! Pair table. A dynamic block whose literal codes are short gets a second primary litlen table
//! of `PRIMARY` entries. It is the litlen one, except where the codes of two literals, n1 bits at
//! `i` and n2 bits at `i >> n1`, fit in `LL_BITS` bits together: there the entry decodes both
//! literals in one lookup and one shift. Its layout: bits 0-4 n1 + n2, bit 6 PAIR_FLAG, bit 7 LIT, bits
//! 8-15 those of the second literal's entry (so the slack is that entry's pull minus n), bits
//! 16-23 the first literal, bits 24-31 the second. So bits 16-31 of any literal entry are its
//! output halfword (a single's second byte is junk), and the loop stores two bytes and advances
//! by one plus PAIR_FLAG. `build_pairs` decides by a model whether a block pays for the table.
//!
//! Slack. stdlib reads at least `max(min length, n)` bits for a litlen symbol, so after a symbol
//! it can have read more than the symbol used. The loop tracks that for the last litlen symbol,
//! `max(0, pull - used)`, and the handoff adds it to the position before it rounds up to bytes.

use std::sync::OnceLock;

pub(crate) const LL_BITS: u32 = 11;
pub(crate) const DIST_BITS: u32 = 8;
const LL_MASK: u64 = (1 << LL_BITS) - 1;
const DIST_MASK: u64 = (1 << DIST_BITS) - 1;
pub(crate) const LINK: u32 = 1 << 5;
pub(crate) const STOP: u32 = 1 << 6;
pub(crate) const LIT: u32 = 1 << 7;
/// With LIT, in the pair table: the entry has a second literal.
const PAIR_FLAG: u32 = 1 << 6;
/// Subtables hold at least two codes each.
pub(crate) const PRIMARY: usize = 1 << LL_BITS;
pub(crate) const LL_SIZE: usize = (1 << LL_BITS) + ((288 / 2) << (15 - LL_BITS));
pub(crate) const DIST_SIZE: usize = (1 << DIST_BITS) + ((32 / 2) << (15 - DIST_BITS));
/// Input bytes from `ip` the loop needs to start a pass, the 8 it loads included: it refills at
/// most three times between checks, so it reads below `ip + 22` (kernel.rs, R1).
pub(crate) const IN_MARGIN: usize = 24;
/// Room past `out_limit` for one pass: two literal steps (4 bytes) and a 258-byte match whose
/// word copies run past its end by up to 31 bytes (kernel.rs, W1), so under 320.
pub(crate) const OUT_MARGIN: usize = 320;
/// The most one pass of the loop writes after its check of the output limit: two literal steps
/// of up to two bytes, then the longest match.
pub(crate) const MAX_STEP: usize = 4 + 258;

/// The tables of one block.
///
/// The three tables are one array, so the loop holds one pointer for them: the litlen table, the
/// distance table, and the primary litlen entries with two-literal entries (see below), valid if
/// `has_pair`.
pub(crate) struct Tables {
    a: Box<[u32; TOTAL]>,
    pub has_pair: bool,
}

const LL_OFF: usize = 0;
const DT_OFF: usize = LL_SIZE;
const PT_OFF: usize = LL_SIZE + DIST_SIZE;
const TOTAL: usize = PT_OFF + PRIMARY;

impl Tables {
    /// The bytes of the boxed table array.
    pub const BYTES: usize = TOTAL * 4;

    pub fn new() -> Self {
        Self {
            a: Box::new([0; TOTAL]),
            has_pair: false,
        }
    }

    pub fn ll(&self) -> &[u32] {
        &self.a[LL_OFF..DT_OFF]
    }

    pub fn dt(&self) -> &[u32] {
        &self.a[DT_OFF..PT_OFF]
    }
}

/// Litlen entries before the code length is added.
const LL_SYMS: [u32; 288] = ll_syms();
const DIST_SYMS: [u32; 32] = dist_syms();

const fn ll_syms() -> [u32; 288] {
    let mut t = [0u32; 288];
    let mut v = 0;
    while v < 288 {
        t[v] = if v < 256 {
            ((v as u32) << 16) | LIT
        } else if v == 256 || v >= 286 {
            ((v as u32) << 16) | STOP
        } else {
            let (base, extra) = if v < 265 {
                (v - 254, 0)
            } else if v < 285 {
                let extra = (v - 261) / 4;
                ((((v - 261) % 4) << extra) + (1 << (extra + 2)) + 3, extra)
            } else {
                (258, 0)
            };
            ((base as u32) << 16) | extra as u32
        };
        v += 1;
    }
    t
}

const fn dist_syms() -> [u32; 32] {
    let mut t = [0u32; 32];
    let mut d = 0;
    while d < 32 {
        t[d] = if d < 4 {
            (d as u32 + 1) << 16
        } else if d < 30 {
            let nb = ((d - 2) >> 1) as u32;
            (((1 << (nb + 1)) + 1 + (((d & 1) as u32) << nb)) << 16) | nb
        } else {
            ((d as u32) << 16) | STOP
        };
        d += 1;
    }
    t
}

/// Fills `t` (the `1 << table_bits` primary entries and the subtables) from code lengths that the
/// careful path accepted: a complete code, or a degenerate or empty one, whose missing codes are
/// STOP entries. `syms` is `LL_SYMS` or `DIST_SYMS`; `min_ll` is stdlib's litlen minimum.
///
/// A complete code fills the primary entries by doubling (as libdeflate builds its
/// tables): the codes in order of length, each stored once at its reversed code in the first
/// `1 << n` entries, and before the codes of length `n + 1` those entries are copied above
/// themselves. An entry of a code longer than `table_bits` is only junk until its link is
/// written. An incomplete code (under two codes) stores every repeat, over STOP entries.
#[allow(clippy::needless_range_loop)]
fn build_table(t: &mut [u32], lengths: &[u8], syms: &[u32], table_bits: u32, min_ll: u32) {
    let mut count = [0usize; 16];
    for &n in lengths {
        count[usize::from(n & 15)] += 1;
    }
    let ncodes = lengths.len() - count[0];
    count[0] = 0;
    let mut next = [0usize; 16];
    let mut code = 0;
    for i in 1..16 {
        code = (code + count[i - 1]) << 1;
        next[i] = code;
    }

    let size = 1usize << table_bits;
    let mask = size - 1;
    // The entry of a symbol of length `n`.
    let entry = |sym: usize, n: u32| {
        let mut e = syms[sym] | (n << 8);
        if e & STOP == 0 {
            let pull = n.max(min_ll);
            e += n | (pull << 12);
            if e & LIT != 0 {
                e |= (pull - n) << 24;
            }
        }
        e
    };

    // Reversed codes, each subtable's widest code, and the symbols in order of length.
    let mut rev = [0usize; 288];
    let mut sub_max = [0u8; 1 << LL_BITS];
    let mut subs = [0u16; 288]; // subtable prefixes, in first-use order
    let mut nsub = 0;
    let mut start = [0usize; 17];
    for n in 1..16 {
        start[n + 1] = start[n] + count[n];
    }
    let mut sorted = [0u16; 288];
    for (sym, &n) in lengths.iter().enumerate() {
        if n == 0 {
            continue;
        }
        let n = usize::from(n & 15);
        let r = usize::from((next[n] as u16).reverse_bits()) >> (16 - n);
        next[n] += 1;
        rev[sym] = r;
        sorted[start[n]] = sym as u16;
        start[n] += 1;
        if n > table_bits as usize {
            if sub_max[r & mask] == 0 {
                subs[nsub] = (r & mask) as u16;
                nsub += 1;
            }
            sub_max[r & mask] = sub_max[r & mask].max(n as u8);
        }
    }

    if ncodes < 2 {
        // Incomplete: the codes that are not there stop the loop.
        t[..size].fill(STOP);
        for (sym, &n) in lengths.iter().enumerate() {
            let n = u32::from(n & 15);
            if n == 0 || n > table_bits {
                continue;
            }
            let e = entry(sym, n);
            let mut i = rev[sym];
            while i < size {
                t[i] = e;
                i += 1 << n;
            }
        }
    } else {
        // `sorted` ends with the codes longer than `table_bits`; the shorter ones come first.
        let short = start[table_bits as usize];
        let mut cur = 0u32;
        for &sym in &sorted[..short] {
            let sym = usize::from(sym);
            let n = u32::from(lengths[sym] & 15);
            while cur < n {
                if cur > 0 {
                    t.copy_within(..1 << cur, 1 << cur);
                }
                cur += 1;
            }
            t[rev[sym]] = entry(sym, n);
        }
        while cur < table_bits {
            if cur > 0 {
                t.copy_within(..1 << cur, 1 << cur);
            }
            cur += 1;
        }
    }

    // The links, then the subtables.
    let mut free = size;
    for &p in &subs[..nsub] {
        let w = u32::from(sub_max[usize::from(p)]) - table_bits;
        t[usize::from(p)] = ((free as u32) << 16) | LINK | (w << 8);
        free += 1 << w;
    }
    for (sym, &n) in lengths.iter().enumerate() {
        let n = u32::from(n & 15);
        if n <= table_bits {
            continue;
        }
        let e = entry(sym, n);
        let r = rev[sym];
        let p = r & mask;
        let (base, w) = ((t[p] >> 16) as usize, t[p] >> 8 & 15);
        let mut i = r >> table_bits;
        while i < 1 << w {
            t[base + i] = e;
            i += 1 << (n - table_bits);
        }
    }
}

/// The tables of a dynamic block, from the lengths the careful path accepted.
pub(crate) fn build(t: &mut Tables, ll_lengths: &[u8], dist_lengths: &[u8], min_ll: u32) {
    let (ll, rest) = t.a.split_at_mut(DT_OFF);
    let (dt, pair) = rest.split_at_mut(DIST_SIZE);
    build_table(ll, ll_lengths, &LL_SYMS, LL_BITS, min_ll);
    build_table(dt, dist_lengths, &DIST_SYMS, DIST_BITS, 0);
    t.has_pair = build_pairs(pair, ll, ll_lengths);
}

/// Fills `p` from the litlen table `t` and the lengths that built it, and reports whether the
/// block should use it: false leaves `p` unset (deflate-go `buildPairs`, mode auto).
#[allow(clippy::needless_range_loop)]
fn build_pairs(p: &mut [u32], t: &[u32], lengths: &[u8]) -> bool {
    let lits = &lengths[..256];
    let mut cnt = [0usize; 16];
    for &n in lits {
        cnt[usize::from(n & 15)] += 1;
    }
    let min_lit = (1..16).find(|&n| cnt[n] != 0).unwrap_or(16);
    // The longest first literal.
    let ll_bits = LL_BITS as usize;
    if ll_bits < 2 * min_lit {
        return false;
    }
    let max_n = ll_bits - min_lit;
    // Shares of 1 << 22: literal symbols, and pairs if symbols were independent with p = 2^-n.
    // ponytail: deflate-go's fixed model (a pair saves ~16x what a single loses, the build costs
    // ~3/8 cycle per symbol at typical block sizes); refit it from the bench numbers.
    let (mut lit, mut pair) = (0u64, 0u64);
    for n in 1..16 {
        lit += (cnt[n] as u64) << (22 - n);
        for n2 in min_lit..=ll_bits.saturating_sub(n) {
            pair += ((cnt[n] * cnt[n2]) as u64) << (22 - n - n2);
        }
    }
    if 16 * pair < lit + (3 << 19) {
        return false;
    }

    // Canonical codes: the rank among the literals of length n, plus base[n]. Sort the literals
    // by length into rs.
    let mut all = cnt;
    for &n in &lengths[256..] {
        all[usize::from(n & 15)] += 1;
    }
    all[0] = 0;
    let (mut base, mut off) = ([0usize; 16], [0usize; 16]);
    let mut lim = [0usize; LL_BITS as usize + 1]; // rs[..lim[n]]: lengths <= n
    let (mut code, mut k) = (0usize, 0usize);
    for n in 1..16 {
        code = (code + all[n - 1]) << 1;
        off[n] = k;
        base[n] = code.wrapping_sub(k);
        k += cnt[n];
        if n <= ll_bits {
            lim[n] = k;
        }
    }
    let mut rs = [0u16; 256];
    for &n in lits {
        let n = usize::from(n & 15);
        if n == 0 {
            continue;
        }
        let i = off[n];
        off[n] = i + 1;
        rs[i] = (base[n].wrapping_add(i) as u16).reverse_bits() >> (16 - n);
    }

    p[..PRIMARY].copy_from_slice(&t[..PRIMARY]);
    for &r1 in &rs[..lim[max_n]] {
        let e1 = t[usize::from(r1)];
        let n1 = e1 & 31;
        for &r2 in &rs[..lim[(LL_BITS - n1) as usize]] {
            let e2 = t[usize::from(r2)];
            let e = ((e1 & 31) + (e2 & 31))
                | LIT
                | PAIR_FLAG
                | (e2 & 0xff00)
                | (e1 & 0x00ff_0000)
                | ((e2 << 8) & 0xff00_0000);
            let step = 1usize << (n1 + (e2 & 31));
            let mut i = usize::from(r1) | (usize::from(r2) << n1);
            while i < PRIMARY {
                p[i] = e;
                i += step;
            }
        }
    }
    true
}

/// The fixed codes of RFC 1951 section 3.2.6.
pub(crate) fn fixed() -> &'static Tables {
    static FIXED: OnceLock<Tables> = OnceLock::new();
    FIXED.get_or_init(|| {
        let mut lit = [8u8; 288];
        lit[144..256].fill(9);
        lit[256..280].fill(7);
        let mut t = Tables::new();
        build(&mut t, &lit, &[5; 32], 7);
        t.has_pair = false; // 8-bit literals never pair
        t
    })
}

/// Where the loop is: Go's `fastState`. The output position is the length of the output `Vec`.
pub(crate) struct State {
    /// Out: the input bytes loaded into `bitbuf`; the loop starts at the front of its input.
    pub ip: usize,
    /// Bit 0 is the next unread input bit.
    pub bitbuf: u64,
    /// Valid bits in `bitbuf`, below 64; bits above are zero or input.
    pub bits: u32,
    /// Out: `max(0, pull - used)` of the last litlen symbol, 0 if there was none.
    pub slack: u32,
}

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod avx2;
#[allow(unsafe_code)]
mod kernel;

/// Decodes from `inp` onto the end of `out` while at least `IN_MARGIN` input bytes are left and
/// `out.len()` is at most `out_limit` at the start of a pass; matches reach back to `hist` at the
/// lowest. The loop writes into the spare capacity of `out`, which must hold `out_limit +
/// OUT_MARGIN` bytes: if it does not (or `hist > out.len()`), nothing happens.
pub(crate) fn run(
    inp: &[u8],
    out: &mut Vec<u8>,
    out_limit: usize,
    hist: usize,
    t: &Tables,
    s: &mut State,
) {
    #[cfg(target_arch = "x86_64")]
    if avx2::run(inp, out, out_limit, hist, t, s) {
        return;
    }
    if t.has_pair {
        kernel::run_impl::<true, false>(inp, out, out_limit, hist, t, s);
    } else {
        kernel::run_impl::<false, false>(inp, out, out_limit, hist, t, s);
    }
}

/// The tier of the loop that `run` takes: "avx2" or "generic".
pub(crate) fn tier() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    if avx2::available() {
        return "avx2";
    }
    "generic"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// CI x86_64 runners have AVX2 and BMI2: the loop that runs there is the one the tests exercise.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_detected() {
        assert!(
            is_x86_feature_detected!("avx2")
                && is_x86_feature_detected!("bmi1")
                && is_x86_feature_detected!("bmi2")
        );
    }

    fn xorshift(st: &mut u64) -> u64 {
        *st ^= *st << 13;
        *st ^= *st >> 7;
        *st ^= *st << 17;
        *st
    }

    const LBASE: [usize; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEXT: [u32; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DBASE: [usize; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DEXT: [u32; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];

    /// A DEFLATE bit writer for fixed-code blocks.
    #[derive(Default)]
    struct Bw {
        out: Vec<u8>,
        acc: u64,
        n: u32,
    }

    impl Bw {
        fn put(&mut self, v: usize, n: u32) {
            self.acc |= (v as u64) << self.n;
            self.n += n;
            while self.n >= 8 {
                self.out.push(self.acc as u8);
                self.acc >>= 8;
                self.n -= 8;
            }
        }
        fn code(&mut self, c: u32, n: u32) {
            self.put((c.reverse_bits() >> (32 - n)) as usize, n);
        }
        fn sym(&mut self, v: u32) {
            match v {
                0..=143 => self.code(0x30 + v, 8),
                144..=255 => self.code(0x190 + v - 144, 9),
                256..=279 => self.code(v - 256, 7),
                _ => self.code(0xc0 + v - 280, 8),
            }
        }
        fn lit(&mut self, b: u8) {
            self.sym(b.into());
        }
        fn mat(&mut self, len: usize, dist: usize) {
            let i = LBASE.iter().rposition(|&b| b <= len).unwrap();
            self.sym(257 + i as u32);
            self.put(len - LBASE[i], LEXT[i]);
            let j = DBASE.iter().rposition(|&b| b <= dist).unwrap();
            self.code(j as u32, 5);
            self.put(dist - DBASE[j], DEXT[j]);
        }
        fn finish(mut self) -> Vec<u8> {
            if self.n > 0 {
                self.out.push(self.acc as u8);
            }
            self.out
        }
    }

    /// A small fixed block (Miri runs it): matches of short periods at an output position below
    /// 16, then every copy path of the loop, then runs of matches of one period, then 32 bytes of trailing junk so that the loop, not
    /// the careful path, decodes the end.
    fn fixed_stream() -> Vec<u8> {
        let mut st = 1;
        let mut w = Bw::default();
        w.put(1, 1);
        w.put(1, 2);
        for _ in 0..4 {
            w.lit(xorshift(&mut st) as u8);
        }
        w.mat(10, 3);
        w.mat(5, 2);
        for _ in 0..36 {
            w.lit(xorshift(&mut st) as u8);
        }
        for dist in [1, 2, 3, 5, 7, 8, 15, 16, 17, 31, 32, 33, 40] {
            for len in [3, 16, 17, 33, 258] {
                w.mat(len, dist);
                w.lit(b'x');
            }
        }
        // Each match of a run starts where the one before ended, at every phase of the period.
        for dist in [2, 3, 6, 8, 9] {
            for len in [5, 7, 258, 4, 3, 16] {
                w.mat(len, dist);
            }
            w.lit(b'y');
        }
        w.sym(256);
        let mut s = w.finish();
        s.extend([0; 32]);
        s
    }

    /// The loop (writing into uninitialised spare capacity) against the careful path, with
    /// bytes already in `out` and with a limit. Miri runs this one (`miri_` names).
    #[test]
    fn miri_fast_loop_matches_careful() {
        let s = fixed_stream();
        let (l, c) = (crate::Limits::default(), AtomicBool::new(false));
        for pre in [0u8, 5, 17] {
            let prefix: Vec<u8> = (0..pre).collect();
            for n in [usize::MAX, 1000] {
                let (mut fast, mut slow) = (prefix.clone(), prefix.clone());
                let rf = crate::decode_limit_into(&s, n, &mut fast, &l, &c);
                let rs = crate::decode_careful_into(&s, n, &mut slow, &l, &c);
                assert_eq!(rf, rs, "pre {pre} n {n}");
                assert!(rf.is_ok() && fast == slow, "pre {pre} n {n}");
            }
        }
    }

    /// zlib with the Adler-32 summed during the decode: after each loop call and in one pass
    /// with the copy of a stored block. Miri runs it.
    #[test]
    fn miri_zlib_sums_fused() {
        let (l, c) = (crate::Limits::default(), AtomicBool::new(false));
        let fixed = fixed_stream();
        // Without the junk: the Adler-32 follows the stream.
        let raw = &fixed[..fixed.len() - 32];
        let mut st = 7;
        let mut want: Vec<u8> = (0..300).map(|_| xorshift(&mut st) as u8).collect();
        // A stored block of those 300 bytes, then the fixed block.
        let mut body = vec![0u8, 44, 1, !44, !1];
        body.extend(&want);
        body.extend(raw);
        want.extend(crate::decode(raw, &l, &c).unwrap());
        let sum = crate::adler32::update(1, &want, &l).unwrap();
        let mut z = vec![0x78u8, 0x01];
        z.extend(&body);
        z.extend(sum.to_be_bytes());
        assert_eq!(crate::zlib::decode(&z, &l, &c), Ok(want.clone()));
        let n = z.len();
        z[n - 1] ^= 1;
        let mut got = Vec::new();
        let r = crate::zlib::decode_into(&z, &mut got, &l, &c);
        assert_eq!(r, Err(crate::Error::Checksum));
        assert!(got == want);
    }

    /// Tables of a dynamic block whose 4-bit literal codes pair: 15 literals of 4 bits, the end
    /// of block and lengths 3 to 5 of 6 bits, 32 distance codes of 5 bits.
    fn pair_tables() -> Tables {
        let mut ll = [0u8; 288];
        ll[usize::from(b'a')..usize::from(b'a') + 15].fill(4);
        ll[256..260].fill(6);
        let mut t = Tables::new();
        build(&mut t, &ll, &[5; 32], 6);
        assert!(t.has_pair);
        t
    }

    /// The litlen lengths of a dynamic block: 15 literals of 4 bits (they pair), then a chain of
    /// codes up to 12 bits, so the two longest are in a subtable, and the end of block and lengths
    /// 3 and 4 of 6 bits. A complete code.
    fn dyn_lens() -> [u8; 259] {
        let mut l = [0u8; 259];
        l[97..112].fill(4);
        l[112..119].copy_from_slice(&[7, 8, 9, 10, 11, 12, 12]);
        l[256..259].fill(6);
        l
    }

    /// A small dynamic block (Miri runs it) of `dyn_lens` and two distance codes (1 and 2): the
    /// pair table and the subtable paths of the loop, then 32 bytes of trailing junk.
    #[allow(clippy::needless_range_loop)]
    fn dyn_stream() -> Vec<u8> {
        let lens = dyn_lens();
        let (mut cnt, mut next, mut c) = ([0u32; 16], [0u32; 16], 0);
        lens.iter().for_each(|&n| cnt[usize::from(n)] += 1);
        cnt[0] = 0;
        for i in 1..16 {
            c = (c + cnt[i - 1]) << 1;
            next[i] = c;
        }
        let codes: Vec<u32> = lens
            .iter()
            .map(|&n| {
                next[usize::from(n)] += 1;
                next[usize::from(n)] - 1
            })
            .collect();
        let mut w = Bw::default();
        w.put(1, 1);
        w.put(2, 2);
        // 259 litlen codes, 2 distance codes, 19 code length codes: all 16 lengths of 4 bits.
        w.put(2, 5);
        w.put(1, 5);
        w.put(15, 4);
        for i in 0..19 {
            w.put(if i < 3 { 0 } else { 4 }, 3);
        }
        for &n in lens.iter().chain(&[1, 1]) {
            w.code(u32::from(n), 4);
        }
        let mut st = 3;
        for i in 0..150 {
            let r = xorshift(&mut st);
            if i < 2 || !r.is_multiple_of(6) {
                let b = 97 + (r >> 8) as usize % 22;
                w.code(codes[b], lens[b].into());
            } else {
                let s = 257 + (r >> 8) as usize % 2;
                w.code(codes[s], lens[s].into());
                w.code((r >> 16) as u32 & 1, 1);
            }
        }
        w.code(codes[256], lens[256].into());
        let mut s = w.finish();
        s.extend([0; 32]);
        s
    }

    /// The loop on a dynamic block with pair entries and subtable links (the two-byte literal
    /// store, LINK) against the careful path, with a prefix and with a limit. Miri runs it.
    #[test]
    fn miri_dynamic_pair_link_matches_careful() {
        let mut t = Tables::new();
        build(&mut t, &dyn_lens(), &[1, 1], 4);
        assert!(t.has_pair && t.ll()[..PRIMARY].iter().any(|e| e & LINK != 0));
        let s = dyn_stream();
        let (l, c) = (crate::Limits::default(), AtomicBool::new(false));
        for pre in [0u8, 5] {
            let prefix: Vec<u8> = (0..pre).collect();
            for n in [usize::MAX, 100] {
                let (mut fast, mut slow) = (prefix.clone(), prefix.clone());
                let rf = crate::decode_limit_into(&s, n, &mut fast, &l, &c);
                let rs = crate::decode_careful_into(&s, n, &mut slow, &l, &c);
                assert_eq!(rf, rs, "pre {pre} n {n}");
                assert!(rf.is_ok() && fast == slow, "pre {pre} n {n}");
            }
        }
    }

    /// One call of the loop on `inp` after `hist` bytes of history: the output and the state.
    fn one_call(
        inp: &[u8],
        hist: &[u8],
        t: &Tables,
        avx2: bool,
    ) -> (Vec<u8>, usize, u64, u32, u32) {
        let mut out = hist.to_vec();
        out.reserve_exact(4096 + OUT_MARGIN);
        let out_limit = hist.len() + 4096;
        let mut s = State {
            ip: 0,
            bitbuf: 0,
            bits: 0,
            slack: 0,
        };
        if avx2 {
            #[cfg(target_arch = "x86_64")]
            assert!(avx2::run(inp, &mut out, out_limit, 0, t, &mut s));
        } else if t.has_pair {
            kernel::run_impl::<true, false>(inp, &mut out, out_limit, 0, t, &mut s);
        } else {
            kernel::run_impl::<false, false>(inp, &mut out, out_limit, 0, t, &mut s);
        }
        (out, s.ip, s.bitbuf, s.bits, s.slack)
    }

    /// The AVX2 tier against the generic loop, bit-exact: random input bits through the fixed
    /// and a pair table, after 32 KiB of random history plus 0 to 6 bytes (unaligned output),
    /// from unaligned input starts.
    #[test]
    fn loop_tiers_match() {
        assert!(tier() == "generic" || cfg!(target_arch = "x86_64"));
        if tier() == "generic" {
            return;
        }
        let pair = pair_tables();
        let mut st = 0x9e37_79b9_7f4a_7c15;
        for round in 0..400usize {
            let hist: Vec<u8> = (0..32768 + round % 7)
                .map(|_| xorshift(&mut st) as u8)
                .collect();
            let buf: Vec<u8> = (0..700).map(|_| xorshift(&mut st) as u8).collect();
            let inp = &buf[round % 5..];
            for t in [fixed(), &pair] {
                assert!(
                    one_call(inp, &hist, t, false) == one_call(inp, &hist, t, true),
                    "round {round}"
                );
            }
        }
    }
}
