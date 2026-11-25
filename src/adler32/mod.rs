//! Adler-32. `scalar` is the reference; the SIMD tiers (AVX-512 VNNI and AVX2 on x86_64, NEON on
//! aarch64) sit beside it. Each tier has two kernels: the sum, and a copy that sums what it
//! copies in the same pass. The tier is picked once; `DEFLATE_SIMD=scalar` (and Miri) force the
//! scalar one.
//!
//! The kernels are safe `#[target_feature]` fns. `unsafe` is only in each tier's two calls into
//! them, next to its `detect`, in each tier's `mem` module of loads and stores, and in `extend`
//! here (`set_len` after the copy).

#[cfg(target_arch = "x86_64")]
mod avx2;
#[cfg(target_arch = "aarch64")]
mod neon;
mod scalar;
#[cfg(target_arch = "x86_64")]
mod vnni;

use crate::{Error, Limits};
use std::mem::MaybeUninit;
use std::sync::OnceLock;

#[doc(hidden)]
pub type Kernel = fn(u32, &[u8]) -> u32;
/// Copies `src` into `dst` (as long) and returns the Adler-32 of `adler` followed by `src`.
type CopyKernel = fn(u32, &[u8], &mut [MaybeUninit<u8>]) -> u32;

/// One tier: its name and its two kernels. Only a tier's `detect` makes one, after the CPU check
/// its kernels need.
#[derive(Clone, Copy)]
struct Tier {
    name: &'static str,
    sum: Kernel,
    copy: CopyKernel,
}

const SCALAR: Tier = Tier {
    name: "scalar",
    sum: scalar::update,
    copy: scalar::copy_sum,
};

/// The best SIMD tier this CPU supports, if any.
#[cfg(target_arch = "x86_64")]
fn detect() -> Option<Tier> {
    vnni::detect().or_else(avx2::detect)
}
#[cfg(target_arch = "aarch64")]
use neon::detect;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn detect() -> Option<Tier> {
    None
}

/// Every SIMD tier this CPU can run, whichever `selected` picks.
#[cfg(any(test, fuzzing))]
fn simd_tiers() -> Vec<Tier> {
    let mut v = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        v.extend(avx2::detect());
        v.extend(vnni::detect());
    }
    #[cfg(target_arch = "aarch64")]
    v.extend(neon::detect());
    v
}

/// Every SIMD sum kernel this CPU can run, by name, whichever `update` picks. Not API: it exists
/// only in tests and the fuzz build (`--cfg fuzzing`), which compare each with the scalar code,
/// so no release build hands out a kernel that takes no `Limits`.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub fn kernels() -> Vec<(&'static str, Kernel)> {
    simd_tiers().into_iter().map(|t| (t.name, t.sum)).collect()
}

/// The copy kernel of every tier this CPU can run, scalar first, by name: each appends `src` to
/// a copy of `prefix` with room reserved, and returns the result and the Adler-32 of `adler`
/// followed by `src`. Not API: tests and the fuzz build only.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub fn copy_sum_tiers(prefix: &[u8], src: &[u8], adler: u32) -> Vec<(&'static str, Vec<u8>, u32)> {
    let mut tiers = vec![SCALAR];
    tiers.extend(simd_tiers());
    tiers
        .into_iter()
        .map(|t| {
            let mut out = Vec::with_capacity(prefix.len() + src.len());
            out.extend_from_slice(prefix);
            let a = extend(t.copy, &mut out, src, adler);
            (t.name, out, a)
        })
        .collect()
}

/// Returns the Adler-32 checksum of the input that gave `adler`, followed by `p`.
/// The checksum of no input is 1. Equals Go `hash/adler32` for every `adler` and `p`.
/// `p` longer than `limits.max_output` is [`Error::OutputLimit`]: it is the bytes of one decode.
///
/// # Errors
///
/// Only [`Error::OutputLimit`]. It allocates nothing and does not look at a cancel flag.
pub fn update(adler: u32, p: &[u8], limits: &Limits) -> Result<u32, Error> {
    if p.len() > limits.max_output {
        return Err(Error::OutputLimit);
    }
    Ok(sum(adler, p))
}

/// [`update`] without the limit: the callers in this crate hold bytes that a decode already
/// charged against `max_output`.
pub(crate) fn sum(adler: u32, p: &[u8]) -> u32 {
    (selected().sum)(adler, p)
}

/// Appends `src` to `out` and returns the Adler-32 of `adler` followed by `src`, in one pass over
/// `src`. `out` has room for `src` (the caller reserved it, charged to `max_alloc`); without it,
/// this copies, then sums.
pub(crate) fn extend_sum(out: &mut Vec<u8>, src: &[u8], adler: u32) -> u32 {
    extend(selected().copy, out, src, adler)
}

#[allow(unsafe_code)]
fn extend(copy: CopyKernel, out: &mut Vec<u8>, src: &[u8], adler: u32) -> u32 {
    let len = out.len();
    let Some(dst) = out.spare_capacity_mut().get_mut(..src.len()) else {
        out.extend_from_slice(src);
        return sum(adler, src);
    };
    let a = copy(adler, src, dst);
    // SAFETY: `dst` is the `src.len()` bytes of spare capacity after `len`, and a copy kernel
    // writes every byte of a `dst` as long as its `src`: the scalar one byte by byte, the SIMD
    // ones each whole chunk, then the scalar tail (the cross-tier tests and the `kernels` fuzz
    // target check the bytes).
    unsafe { out.set_len(len + src.len()) };
    a
}

/// The tier `sum` and `extend_sum` run: the best SIMD one this CPU has, or scalar
/// (`DEFLATE_SIMD=scalar`, Miri, or no SIMD).
fn selected() -> Tier {
    static TIER: OnceLock<Tier> = OnceLock::new();
    *TIER.get_or_init(|| {
        if cfg!(miri) || std::env::var_os("DEFLATE_SIMD").is_some_and(|v| v == "scalar") {
            SCALAR
        } else {
            detect().unwrap_or(SCALAR)
        }
    })
}

/// The name of the tier that `selected` picked.
pub(crate) fn tier() -> &'static str {
    selected().name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(state: &mut u64) -> u8 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 24) as u8
    }

    /// `sum` runs the best SIMD tier, not scalar, unless `DEFLATE_SIMD=scalar` says so.
    #[test]
    fn dispatch_picks_tier() {
        let scalar = std::env::var_os("DEFLATE_SIMD").is_some_and(|v| v == "scalar");
        let want = if scalar {
            "scalar"
        } else {
            detect().map_or("scalar", |t| t.name)
        };
        assert_eq!(tier(), want);
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        assert!(scalar || tier() != "scalar", "SIMD expected on this CPU");
    }

    /// Every SIMD kernel this CPU runs against scalar: random and all-0xff data, edge lengths,
    /// unaligned starts.
    #[test]
    fn simd_matches_scalar() {
        #[cfg(target_arch = "aarch64")]
        assert!(detect().is_some(), "NEON expected on aarch64");
        // CI x86_64 runners have AVX2; AVX-512 VNNI is tested when the CPU has it.
        #[cfg(target_arch = "x86_64")]
        assert!(avx2::detect().is_some(), "AVX2 expected on x86_64");
        let tiers: Vec<Kernel> = kernels().into_iter().map(|(_, k)| k).collect();
        let mut st = 0x9e37_79b9_7f4a_7c15u64;
        let rand: Vec<u8> = (0..140_100).map(|_| xorshift(&mut st)).collect();
        let ff = vec![0xffu8; 140_100];
        let lens = [
            0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 191, 192, 193, 255, 256, 257,
            320, 384, 5535, 5536, 5537, 5551, 5552, 5553, 11072, 11073, 65535, 65536, 65537, 65600,
            65664, 100_000, 131_072, 131_136,
        ];
        for k in tiers {
            for buf in [&rand, &ff] {
                for off in [0, 1, 3] {
                    for &n in &lens {
                        for adler in [1, 0, 0xfff0_fff0, 0xffff_ffff] {
                            let p = &buf[off..off + n];
                            assert_eq!(k(adler, p), scalar::update(adler, p), "off {off} n {n}");
                        }
                    }
                }
            }
            for n in 0..300 {
                let p = &rand[n..2 * n];
                assert_eq!(k(1, p), scalar::update(1, p), "n {n}");
            }
        }
    }

    /// Every copy kernel this CPU runs against the scalar one, bit-exact: the bytes appended and
    /// the sum, for edge lengths around each lane width (16, 32, 64) and the block sizes, from
    /// unaligned sources into unaligned destinations.
    #[test]
    fn copy_matches_scalar() {
        let mut st = 0x2545_f491_4f6c_dd1du64;
        let rand: Vec<u8> = (0..140_100).map(|_| xorshift(&mut st)).collect();
        let ff = vec![0xffu8; 140_100];
        let mut lens: Vec<usize> = vec![0, 1, 2, 300, 5535, 5536, 5537, 5552, 5553, 65536, 65601];
        for lane in [16, 32, 64, 128, 256] {
            lens.extend([lane - 1, lane, lane + 1, 2 * lane + 1, 3 * lane]);
        }
        let n_tiers = copy_sum_tiers(&[], &[], 1).len();
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        assert!(n_tiers > 1, "no SIMD copy kernel on this CPU");
        for buf in [&rand, &ff] {
            for (off, pre) in [(0, 0), (1, 3), (3, 1), (5, 31)] {
                for &n in &lens {
                    let src = &buf[off..off + n];
                    let prefix = &buf[..pre];
                    let mut want = prefix.to_vec();
                    want.extend_from_slice(src);
                    for adler in [1, 0xfff0_fff0] {
                        let all = copy_sum_tiers(prefix, src, adler);
                        assert_eq!(all.len(), n_tiers);
                        for (name, out, a) in all {
                            assert!(out == want, "{name} off {off} n {n}: bytes");
                            assert_eq!(a, scalar::update(adler, src), "{name} off {off} n {n}");
                        }
                    }
                }
            }
        }
    }

    /// The copy into spare capacity and `set_len`, through the tier `extend_sum` runs (scalar
    /// under Miri, which runs this one), and the fallback without room.
    #[test]
    fn miri_extend_sum() {
        let mut st = 3u64;
        let src: Vec<u8> = (0..200).map(|_| xorshift(&mut st)).collect();
        for n in [0, 1, 15, 16, 17, 63, 64, 65, 200] {
            let mut out = vec![7u8; 3];
            out.reserve_exact(n);
            let a = extend_sum(&mut out, &src[..n], 1);
            assert_eq!(a, scalar::update(1, &src[..n]));
            assert!(out[..3] == [7; 3] && out[3..] == src[..n]);
            let mut full = vec![7u8; 3];
            full.shrink_to_fit();
            assert_eq!(extend_sum(&mut full, &src[..n], 1), a);
            assert!(full == out);
        }
    }
}
