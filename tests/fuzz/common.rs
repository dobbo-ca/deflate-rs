//! Shared by the decode targets (`#[path = "../common.rs"] mod common;`).

/// Once per process: on x86_64 unless `DEFLATE_SIMD=scalar`, the CPU must have AVX2, BMI1 and
/// BMI2, or the `default` leg would fuzz the generic loop only. Logs which tier runs.
pub fn check_tier() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let scalar = std::env::var_os("DEFLATE_SIMD").is_some_and(|v| v == "scalar");
        #[cfg(target_arch = "x86_64")]
        if !scalar {
            assert!(
                is_x86_feature_detected!("avx2")
                    && is_x86_feature_detected!("bmi1")
                    && is_x86_feature_detected!("bmi2"),
                "no AVX2/BMI1/BMI2: the AVX2 fast loop would go unfuzzed"
            );
        }
        eprintln!(
            "decode fast loop fuzzed: {}",
            if scalar || !cfg!(target_arch = "x86_64") {
                "generic"
            } else {
                "avx2"
            }
        );
    });
}
