//! AVX2 Adler-32: 32 bytes per step. Per block, `v1` sums the bytes, `v2` the bytes weighted
//! 32..=1, and `sums` the running `v1` before each step (weighting earlier steps by their
//! distance to the block's end). Lanes stay far below 2^32 for a block of `BLOCK` bytes; the
//! combination and the mod are done in u64. The copy kernel stores each 32 bytes it sums.
//!
//! The kernel is a safe `#[target_feature]` fn; the `unsafe` is in `update` and `copy_sum` (the
//! calls, next to `detect`) and in `mem` (the loads and stores).

use super::Tier;
use std::arch::x86_64::*;
use std::mem::MaybeUninit;

const MOD: u64 = 65521;
/// Bytes per block: a multiple of 32, at most NMAX (5552).
const BLOCK: usize = 5536;

pub(super) fn detect() -> Option<Tier> {
    is_x86_feature_detected!("avx2").then_some(Tier {
        name: "avx2",
        sum: update,
        copy: copy_sum,
    })
}

/// Only reachable through `detect`, which checked AVX2.
#[allow(unsafe_code)]
fn update(adler: u32, p: &[u8]) -> u32 {
    let (c, tail) = p.as_chunks::<32>();
    // SAFETY: the one requirement of the call is that the CPU has AVX2, and `detect` hands out
    // this fn only after `is_x86_feature_detected!("avx2")` saw it.
    let a = unsafe { kernel::<false>(adler, c, &mut []) };
    super::scalar::update(a, tail)
}

/// Only reachable through `detect`, which checked AVX2. `dst` is as long as `src`, so the kernel
/// and the scalar tail write every byte of it.
#[allow(unsafe_code)]
fn copy_sum(adler: u32, src: &[u8], dst: &mut [MaybeUninit<u8>]) -> u32 {
    let (c, tail) = src.as_chunks::<32>();
    let (d, dtail) = dst.as_chunks_mut::<32>();
    // SAFETY: as in `update`: `detect` saw AVX2.
    let a = unsafe { kernel::<true>(adler, c, d) };
    super::scalar::copy_sum(a, tail, dtail)
}

/// Sum of the u32 lanes, as u64.
#[target_feature(enable = "avx2")]
fn hsum(v: __m256i) -> u64 {
    let s = _mm256_add_epi64(
        _mm256_cvtepu32_epi64(_mm256_castsi256_si128(v)),
        _mm256_cvtepu32_epi64(_mm256_extracti128_si256::<1>(v)),
    );
    (_mm256_extract_epi64::<0>(s)
        + _mm256_extract_epi64::<1>(s)
        + _mm256_extract_epi64::<2>(s)
        + _mm256_extract_epi64::<3>(s)) as u64
}

/// The Adler-32 of `adler` followed by `p`; with `COPY`, also stores `p` into `dst` (as long).
#[target_feature(enable = "avx2")]
fn kernel<const COPY: bool>(adler: u32, p: &[[u8; 32]], dst: &mut [[MaybeUninit<u8>; 32]]) -> u32 {
    let (mut s1, mut s2) = (u64::from(adler & 0xffff), u64::from(adler >> 16));
    let w = _mm256_setr_epi8(
        32, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10,
        9, 8, 7, 6, 5, 4, 3, 2, 1,
    );
    let zero = _mm256_setzero_si256();
    let ones = _mm256_set1_epi16(1);
    let mut d = dst.iter_mut();
    for block in p.chunks(BLOCK / 32) {
        let (mut v1, mut v2, mut sums) = (zero, zero, zero);
        for c in block {
            let v = mem::load(c);
            if COPY && let Some(x) = d.next() {
                mem::store(x, v);
            }
            sums = _mm256_add_epi32(sums, v1);
            v1 = _mm256_add_epi32(v1, _mm256_sad_epu8(v, zero));
            v2 = _mm256_add_epi32(v2, _mm256_madd_epi16(_mm256_maddubs_epi16(v, w), ones));
        }
        s2 += s1 * (32 * block.len()) as u64 + 32 * hsum(sums) + hsum(v2);
        s1 += hsum(v1);
        s1 %= MOD;
        s2 %= MOD;
    }
    ((s2 << 16) | s1) as u32
}

/// The loads and stores: the array types are the bounds.
#[allow(unsafe_code)]
mod mem {
    use std::arch::x86_64::*;
    use std::mem::MaybeUninit;

    #[inline]
    #[target_feature(enable = "avx2")]
    pub(super) fn load(a: &[u8; 32]) -> __m256i {
        // SAFETY: `a` is 32 readable bytes, and loadu needs no alignment; the CPU has AVX2, as
        // this fn's target feature requires of its callers.
        unsafe { _mm256_loadu_si256(a.as_ptr().cast()) }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    pub(super) fn store(a: &mut [MaybeUninit<u8>; 32], v: __m256i) {
        // SAFETY: `a` is 32 writable bytes, and storeu needs no alignment; AVX2 as above.
        unsafe { _mm256_storeu_si256(a.as_mut_ptr().cast(), v) }
    }
}
