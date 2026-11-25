//! The decode loop compiled with AVX2, BMI1 and BMI2 enabled: the shifts and masks of
//! `run_impl` compile to SHRX, SHLX and BZHI (as deflate-go `fast_amd64.go`), the 32-byte match
//! copies to one load and one store, and its `AVX2` instantiation copies a short-period match
//! with PSHUFB and 32-byte stores. It is the same code: `run_impl` is inlined into a
//! safe `#[target_feature]` function, and the call to it, next to the detection, is the only
//! `unsafe` here.

use super::{State, Tables, kernel::run_impl};
use std::sync::OnceLock;

/// True if the CPU has AVX2, BMI1 and BMI2 and `DEFLATE_SIMD=scalar` is not set (never under
/// Miri, which runs the generic loop).
pub(super) fn available() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let scalar = std::env::var_os("DEFLATE_SIMD").is_some_and(|v| v == "scalar");
        !cfg!(miri)
            && !scalar
            && is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi1")
            && is_x86_feature_detected!("bmi2")
    })
}

/// Runs the loop with AVX2 if it is available, and says so.
pub(super) fn run(
    inp: &[u8],
    out: &mut Vec<u8>,
    out_limit: usize,
    hist: usize,
    t: &Tables,
    s: &mut State,
) -> bool {
    if !available() {
        return false;
    }
    // SAFETY: the one requirement of calling an `avx2,bmi1,bmi2` target-feature fn is that the
    // CPU has them, and `available()` is true only after `is_x86_feature_detected!` saw all three.
    unsafe {
        if t.has_pair {
            run_avx2::<true>(inp, out, out_limit, hist, t, s);
        } else {
            run_avx2::<false>(inp, out, out_limit, hist, t, s);
        }
    }
    true
}

#[target_feature(enable = "avx2,bmi1,bmi2")]
fn run_avx2<const PAIR: bool>(
    inp: &[u8],
    out: &mut Vec<u8>,
    out_limit: usize,
    hist: usize,
    t: &Tables,
    s: &mut State,
) {
    run_impl::<PAIR, true>(inp, out, out_limit, hist, t, s);
}
