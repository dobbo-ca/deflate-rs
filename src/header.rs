//! Dynamic block headers read from a byte slice with a 64-bit bit buffer (the
//! libdeflate-style decoder): the code lengths are decoded with a 7-bit table of the code length
//! code, a lookup and a shift per symbol, where the careful path (`inflate.rs`) reads one input
//! byte at a time through `BufRead`.
//!
//! The reader also keeps the read position stdlib's reader would have: `pulled`, the bits that
//! `need`/`huffSym` of Go's careful path would have taken from the input by now. `take` and `sym`
//! move it exactly as Go does (whole bytes, as late as possible, and `huffSym`'s lookahead to the
//! code length code's minimum), so after a header the caller hands the careful path the same `b`,
//! `nb` and read offset as if it had read the header itself. Anything this reader does not take
//! (an error of any kind, or the end of the slice) returns `None` and consumes nothing: the
//! careful path then reads the same header and reports Go's error.

use crate::huffman::accept_lengths;

pub(crate) const NUM_CODES: usize = 19;
pub(crate) const MAX_NUM_LIT: usize = 286;
pub(crate) const MAX_NUM_DIST: usize = 30;
pub(crate) const CODE_ORDER: [usize; NUM_CODES] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Bits from a slice. Bit 0 of `bitbuf` is bit `pos`; `avail` bits of it are loaded.
pub(crate) struct SliceBits<'a> {
    buf: &'a [u8],
    /// Bytes of `buf` loaded into `bitbuf`.
    next: usize,
    bitbuf: u64,
    avail: u32,
    /// Bits consumed, counted from the first bit of the start state.
    pub pos: u64,
    /// Bits Go's careful path would have read from the input (a multiple of 8 past the start
    /// state's bits), counted as `pos`.
    pub pulled: u64,
}

impl<'a> SliceBits<'a> {
    /// The reader at `nb` bits `b` (the careful path's bit buffer), followed by `buf`.
    pub fn new(buf: &'a [u8], b: u64, nb: u32) -> Self {
        Self {
            buf,
            next: 0,
            bitbuf: b & ((1u64 << nb) - 1),
            avail: nb,
            pos: 0,
            pulled: u64::from(nb),
        }
    }

    /// The reader at bit `bit` of `buf` (no careful path state: `pulled` is not used).
    pub fn at(buf: &'a [u8], bit: u64) -> Option<Self> {
        let byte = usize::try_from(bit / 8).ok()?;
        let first = *buf.get(byte)?;
        let skip = (bit % 8) as u32;
        Some(Self {
            buf,
            next: byte + 1,
            bitbuf: u64::from(first >> skip),
            avail: 8 - skip,
            pos: 0,
            pulled: 0,
        })
    }

    /// Loads bytes until at least `n` (at most 56) bits are in `bitbuf`; false at the end of the
    /// slice.
    #[inline]
    fn fill(&mut self, n: u32) -> bool {
        if self.avail >= n {
            return true;
        }
        if let Some(w) = self.buf.get(self.next..self.next + 8) {
            let mut a = [0u8; 8];
            a.copy_from_slice(w);
            self.bitbuf |= u64::from_le_bytes(a) << self.avail;
            let k = (63 - self.avail) / 8;
            self.next += k as usize;
            self.avail += 8 * k;
            // Bits past `avail` are input too, but `take` masks them.
            self.bitbuf &= (1u64 << self.avail) - 1;
            return true;
        }
        while self.avail < n {
            let Some(&c) = self.buf.get(self.next) else {
                return false;
            };
            self.bitbuf |= u64::from(c) << self.avail;
            self.next += 1;
            self.avail += 8;
        }
        true
    }

    /// Go's `need(n)`: the careful path reads whole bytes until it has `n` unread bits.
    #[inline]
    fn go_need(&mut self, n: u32) {
        while self.pulled - self.pos < u64::from(n) {
            self.pulled += 8;
        }
    }

    #[inline]
    fn consume(&mut self, n: u32) {
        self.bitbuf >>= n;
        self.avail -= n;
        self.pos += u64::from(n);
        // `skip`, `align` and `bytes` move `pos` without `go_need` (the parallel decode, which
        // does not use `pulled`): keep `pulled - pos` from wrapping. A no-op after `take` and
        // `sym`, which pull before they consume.
        self.pulled = self.pulled.max(self.pos);
    }

    /// Reads `n` (at most 32) bits.
    #[inline]
    pub fn take(&mut self, n: u32) -> Option<u32> {
        if !self.fill(n) {
            return None;
        }
        self.go_need(n);
        let v = (self.bitbuf & ((1u64 << n) - 1)) as u32;
        self.consume(n);
        Some(v)
    }

    /// Loads as many bits as fit (56 or more, fewer only at the end of the slice) and returns
    /// them with their count: bits above the count are zero.
    #[inline]
    pub fn fill_max(&mut self) -> (u64, u32) {
        let _ = self.fill(56);
        (self.bitbuf, self.avail)
    }

    /// Consumes `n` bits that `fill_max` returned.
    #[inline]
    pub fn skip(&mut self, n: u32) {
        self.consume(n);
    }

    /// After `align`: the next `n` bytes, consumed.
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let at = self.next - (self.avail / 8) as usize;
        let b = self.buf.get(at..at.checked_add(n)?)?;
        self.next = at + n;
        self.bitbuf = 0;
        self.avail = 0;
        self.pos += 8 * n as u64;
        self.pulled = self.pulled.max(self.pos);
        Some(b)
    }

    /// Discards the bits up to the next byte boundary (a stored block).
    pub fn align(&mut self) {
        let r = self.avail % 8;
        self.consume(r);
    }

    /// The bits Go's careful path holds after this reader's reads: `(b, nb)`, `nb` < 32.
    pub fn handoff(&mut self) -> Option<(u32, u32)> {
        let nb = u32::try_from(self.pulled - self.pos).ok()?;
        if nb >= 32 || !self.fill(nb) {
            return None;
        }
        Some(((self.bitbuf & ((1u64 << nb) - 1)) as u32, nb))
    }

    /// The next symbol of the code length code `t` (`min`: its minimum length), as Go's
    /// `huffSym` reads it: it pulls bytes until it has `min` bits, looks the code up on what it
    /// has (zero filled above), and pulls more only if the code is longer. None for a missing
    /// code or the end of the slice.
    #[inline]
    fn sym(&mut self, t: &[u8; 128], lens: &[u8; 128], min: u32) -> Option<u32> {
        if !self.fill(7) {
            return None;
        }
        let mut n = min;
        loop {
            self.go_need(n);
            let have = (self.pulled - self.pos).min(7) as u32;
            let i = (self.bitbuf & ((1u64 << have) - 1)) as usize;
            n = u32::from(lens[i]);
            if u64::from(n) <= self.pulled - self.pos {
                if n == 0 {
                    return None;
                }
                self.consume(n);
                return Some(u32::from(t[i]));
            }
        }
    }
}

/// The 7-bit lookup table of the code length code: symbol and length per index (length 0: no
/// code). None if the lengths are not a complete code (or a degenerate one: the careful path
/// decides those).
fn code_length_table(codebits: &[u8; NUM_CODES]) -> Option<([u8; 128], [u8; 128], u32)> {
    let (min, ok) = accept_lengths(codebits);
    if !ok || min == 0 {
        return None;
    }
    let mut count = [0u32; 8];
    for &n in codebits {
        count[usize::from(n)] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 8];
    let mut code = 0;
    for i in 1..8 {
        code = (code + count[i - 1]) << 1;
        next[i] = code;
    }
    let (mut syms, mut lens) = ([0u8; 128], [0u8; 128]);
    for (s, &n) in codebits.iter().enumerate() {
        if n == 0 {
            continue;
        }
        let c = next[usize::from(n)];
        next[usize::from(n)] += 1;
        let r = (c.reverse_bits() >> (32 - u32::from(n))) as usize;
        for i in (r..128).step_by(1 << n) {
            syms[i] = s as u8;
            lens[i] = n;
        }
    }
    Some((syms, lens, min))
}

/// Reads the rest of a dynamic block header (after its 3 header bits) as Go's `readHuffman`
/// does: the code lengths into `lens` (`nlit` litlen then `ndist` distance) and `codebits`.
/// Returns `(nlit, ndist)`, or None for anything the careful path must decide (any error).
/// It does not check the litlen and distance codes; the caller does, as Go does after the read.
pub(crate) fn read_dynamic(
    s: &mut SliceBits<'_>,
    codebits: &mut [u8; NUM_CODES],
    lens: &mut [u8; MAX_NUM_LIT + MAX_NUM_DIST],
) -> Option<(usize, usize)> {
    let h = s.take(14)?;
    let nlit = (h & 0x1f) as usize + 257;
    let ndist = (h >> 5 & 0x1f) as usize + 1;
    let nclen = (h >> 10) as usize + 4;
    if nlit > MAX_NUM_LIT || ndist > MAX_NUM_DIST {
        return None;
    }
    for &o in &CODE_ORDER[..nclen] {
        codebits[o] = s.take(3)? as u8;
    }
    for &o in &CODE_ORDER[nclen..] {
        codebits[o] = 0;
    }
    let (syms, clens, min) = code_length_table(codebits)?;
    let n = nlit + ndist;
    let mut i = 0;
    while i < n {
        let x = s.sym(&syms, &clens, min)?;
        if x < 16 {
            lens[i] = x as u8;
            i += 1;
            continue;
        }
        let (rep, nb, b) = match x {
            16 => (3, 2, *lens.get(i.checked_sub(1)?)?),
            17 => (3, 3, 0),
            _ => (11, 7, 0),
        };
        let rep = rep + s.take(nb)? as usize;
        if i + rep > n {
            return None;
        }
        lens[i..i + rep].fill(b);
        i += rep;
    }
    Some((nlit, ndist))
}
