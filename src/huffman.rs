//! Port of the Huffman decoding tables of compress/flate inflate.go (see NOTICE).
//!
//! A lookup table of `CHUNK_BITS` bits. Codes shorter than that fill every entry that ends in
//! them; longer codes link to an overflow table. A lookup with missing high bits (zero filled)
//! still gives a lower bound on the code length, which `Bits::huff_sym` relies on.
//! `chunk & COUNT_MASK` is the number of bits; `chunk >> VALUE_SHIFT` is the value or link.

pub(crate) const MAX_CODE_LEN: usize = 16;
pub(crate) const CHUNK_BITS: u32 = 9;
pub(crate) const NUM_CHUNKS: usize = 1 << CHUNK_BITS;
pub(crate) const COUNT_MASK: u32 = 15;
pub(crate) const VALUE_SHIFT: u32 = 4;

pub(crate) struct Huff {
    /// The minimum code length.
    pub min: u32,
    pub chunks: [u32; NUM_CHUNKS],
    /// Overflow tables, `1 << link_bits` entries each.
    pub links: Vec<u32>,
    pub link_bits: u32,
    pub link_mask: u32,
}

impl Huff {
    pub fn new() -> Self {
        Self {
            min: 0,
            chunks: [0; NUM_CHUNKS],
            links: Vec::new(),
            link_bits: 0,
            link_mask: 0,
        }
    }

    /// Builds the tables from code lengths. On success the code is complete (neither over- nor
    /// under-subscribed), except for the degenerate single code of length 1, and an empty code,
    /// which is allowed and fails on first use.
    #[allow(clippy::needless_range_loop)]
    pub fn init(&mut self, lengths: &[u8]) -> bool {
        self.min = 0;
        self.chunks = [0; NUM_CHUNKS];
        self.links.clear();
        self.link_bits = 0;
        self.link_mask = 0;

        let mut count = [0usize; MAX_CODE_LEN];
        let (mut min, mut max) = (0usize, 0usize);
        for &n in lengths {
            let n = usize::from(n);
            if n == 0 {
                continue;
            }
            if min == 0 || n < min {
                min = n;
            }
            max = max.max(n);
            count[n] += 1;
        }
        if max == 0 {
            return true;
        }

        let mut code = 0usize;
        let mut nextcode = [0usize; MAX_CODE_LEN];
        for i in min..=max {
            code <<= 1;
            nextcode[i] = code;
            code += count[i];
        }
        // Accept degenerate single-code codings, as zlib does.
        if code != 1 << max && (code != 1 || max != 1) {
            return false;
        }

        self.min = min as u32;
        let chunk_bits = CHUNK_BITS as usize;
        if max > chunk_bits {
            let num_links = 1usize << (max - chunk_bits);
            self.link_mask = (num_links - 1) as u32;
            self.link_bits = (max - chunk_bits) as u32;
            let link = nextcode[chunk_bits + 1] >> 1;
            self.links.resize((NUM_CHUNKS - link) * num_links, 0);
            for j in link..NUM_CHUNKS {
                let reverse = usize::from((j as u16).reverse_bits()) >> (16 - chunk_bits);
                let off = j - link;
                self.chunks[reverse] = ((off << VALUE_SHIFT) | (chunk_bits + 1)) as u32;
            }
        }

        for (i, &n) in lengths.iter().enumerate() {
            let n = usize::from(n);
            if n == 0 {
                continue;
            }
            let code = nextcode[n];
            nextcode[n] += 1;
            let chunk = ((i << VALUE_SHIFT) | n) as u32;
            let mut reverse = usize::from((code as u16).reverse_bits()) >> (16 - n);
            if n <= chunk_bits {
                let mut off = reverse;
                while off < NUM_CHUNKS {
                    self.chunks[off] = chunk;
                    off += 1 << n;
                }
            } else {
                let j = reverse & (NUM_CHUNKS - 1);
                let value = (self.chunks[j] >> VALUE_SHIFT) as usize;
                let lb = self.link_bits as usize;
                let linktab = &mut self.links[value << lb..(value + 1) << lb];
                reverse >>= chunk_bits;
                let mut off = reverse;
                while off < linktab.len() {
                    linktab[off] = chunk;
                    off += 1 << (n - chunk_bits);
                }
            }
        }
        true
    }
}

/// `Huff::init`'s accept decision without the tables: the minimum code length (0 for an empty
/// code) and whether the code is complete, or a degenerate single code, or empty.
pub(crate) fn accept_lengths(lengths: &[u8]) -> (u32, bool) {
    let mut count = [0usize; MAX_CODE_LEN];
    let (mut min, mut max) = (0usize, 0usize);
    for &n in lengths {
        let n = usize::from(n);
        if n == 0 {
            continue;
        }
        if min == 0 || n < min {
            min = n;
        }
        max = max.max(n);
        count[n] += 1;
    }
    if max == 0 {
        return (0, true);
    }
    let code = count[min..=max].iter().fold(0usize, |c, &k| (c << 1) + k);
    (min as u32, code == 1 << max || (code == 1 && max == 1))
}
