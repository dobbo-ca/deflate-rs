#![no_main]
use libfuzzer_sys::fuzz_target;

fn update(adler: u32, p: &[u8]) -> u32 {
    deflate::adler32::update(adler, p, &deflate::Limits::default()).expect("within the limit")
}

/// The definition of Adler-32, one byte at a time.
fn reference(adler: u32, p: &[u8]) -> u32 {
    let (mut s1, mut s2) = (u64::from(adler & 0xffff), u64::from(adler >> 16));
    for &x in p {
        s1 = (s1 + u64::from(x)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    ((s2 << 16) | s1) as u32
}

/// A runner without AVX2 or AVX-512 VNNI would fuzz fewer tiers: fail instead. Logs every kernel
/// that is fuzzed, and the one `update` runs.
#[cfg(target_arch = "x86_64")]
fn check_tier() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        assert!(
            is_x86_feature_detected!("avx2"),
            "no AVX2: scalar tier only"
        );
        assert!(
            is_x86_feature_detected!("avx512f")
                && is_x86_feature_detected!("avx512bw")
                && is_x86_feature_detected!("avx512vl")
                && is_x86_feature_detected!("avx512vnni"),
            "no AVX-512 VNNI: its kernel would go unfuzzed"
        );
        assert!(
            deflate::adler32::kernels()
                .iter()
                .any(|k| k.0 == "avx512_vnni"),
            "kernels() lacks avx512_vnni"
        );
        log_tiers();
    });
}

/// As above for NEON: a runner that lacks it (or qemu without it) would fuzz only scalar.
#[cfg(target_arch = "aarch64")]
fn check_tier() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        assert!(
            std::arch::is_aarch64_feature_detected!("neon"),
            "no NEON: scalar tier only"
        );
        log_tiers();
    });
}

fn log_tiers() {
    let names: Vec<_> = deflate::adler32::kernels().iter().map(|k| k.0).collect();
    let scalar = std::env::var_os("DEFLATE_SIMD").is_some_and(|v| v == "scalar");
    eprintln!(
        "adler32 kernels fuzzed: {names:?}; update() runs {}",
        if scalar {
            "scalar"
        } else if cfg!(target_arch = "x86_64") {
            "avx512_vnni, else avx2"
        } else {
            "neon"
        }
    );
}

// data = [adler: u32 LE][start: u8][bytes]. Every SIMD kernel this CPU has, from any start (so any
// alignment) and from any initial value, equals the definition, and a split update equals one.
fuzz_target!(|data: &[u8]| {
    check_tier();
    let Some((&[a, b, c, d, start], rest)) = data.split_first_chunk::<5>() else {
        return;
    };
    let adler = u32::from_le_bytes([a, b, c, d]);
    let p = &rest[usize::from(start % 32).min(rest.len())..];
    let sum = update(adler, p);
    assert_eq!(sum, reference(adler, p));
    // Each kernel on its own, not only the one `update` picks (VNNI hides AVX2 on this CPU).
    for (name, k) in deflate::adler32::kernels() {
        assert_eq!(k(adler, p), sum, "{name}");
    }
    let (x, y) = p.split_at(p.len() / 3);
    assert_eq!(update(update(adler, x), y), sum);
});
