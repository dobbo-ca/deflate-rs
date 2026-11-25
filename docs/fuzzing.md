# Fuzzing


`tests/fuzz/` is a cargo-fuzz crate (the only place `libfuzzer-sys` is used): `decode`, `zlib_decode`,
`decode_limit`, `reader` (the reader with any read size, up to 64 KiB, gives the whole-buffer
result, and reads again at the end or after an error give the same answer),
`predictor`, `adler32`, `kernels` and `parallel` (a stream with 4 to 7 threads and chunks of 64 to
4096 bytes, raw or zlib, with `max_alloc` up to 4 MiB, gives the serial decode's result and bytes;
the parameters are the last three bytes, and it is not in the `DEFLATE_SIMD=scalar` matrix). `reader` also takes short input chunks and a preset
dictionary (raw, against a decode of a stored block of the dictionary and the stream; zlib, the
same stream behind an FDICT header and the Adler-32 of its output; a dictionary longer than
`max_output` is `OutputLimit` at the header, and a stream followed by exactly the right trailer
must give no error). `adler32` is the differential target for the SIMD sum kernels: each one this
CPU has (`deflate::adler32::kernels()`: AVX2 and VNNI on x86_64, NEON on aarch64) against the
definition, from any alignment and initial value; it fails on an x86_64 CPU without AVX2, and logs
the kernels it fuzzes. On x86_64 it also fails without AVX-512 VNNI, so that kernel is never
silently unfuzzed. `kernels` is the differential target of the unsafe kernels: the decode loop
against the careful path alone (with a prefix in `out`), and every fused copy + Adler-32 kernel
this CPU has (`deflate::adler32::copy_sum_tiers()`) against a plain copy and the definition.
`decode`, `zlib_decode`, `decode_limit` and `reader` take `max_output` and `max_alloc` from the
input (`reader`: from three bytes after the header, in a mode that reaches below the 100 KiB a
reader needs, where it may stop with `AllocLimit` and what it returned is a prefix of the
whole-buffer decode), and `decode`, `zlib_decode` and `decode_limit` a prefix already in `out`
and the cancel flag (set before the call: Cancelled, nothing appended). They also assert that
the capacity of `out` stays within `max_alloc` (plus the caller's own), so a regression in the
charging fails the run, not only the `-malloc_limit_mb` backstop. `reader` sets the cancel flag
after k reads in another mode, and `predictor` takes a max_output one byte short, a dst one byte
short and a set flag. The matrix of `fuzz.yml` runs five targets (`decode`, `zlib_decode`,
`decode_limit`, `reader` and `kernels`) also with `DEFLATE_SIMD=scalar`, the build of the fast
loop that the AVX2 one hides on x86_64. Not fuzzed: a cancel that arrives mid-decode of a
whole-buffer call (tests/limits.rs has it). The `fuzz-arm64` job of `fuzz.yml` is a matrix over
`adler32` and `kernels` on a native arm64 runner (with the same sanitizer and limits), so
the NEON sum and fused copy kernels are fuzzed; the `adler32` target fails unless NEON is detected. The corpus of each target is kept between
runs of `fuzz.yml` with `actions/cache`. Seeds are all 48 distinct byte strings of deflate-go's
`testdata/fuzz` entries (`tests/fuzz/seeds/`, the `uint8` argument of its `FuzzReader` seeds dropped,
plus six `reader-` seeds of this crate: a 64 KiB read and five zlib dictionary ones, among them a
dictionary longer than `max_output`) and the fixtures; the zlib `exactcap-stored` seed is also a parity input
(`seed-<name>.zlib`). `.github/workflows/fuzz.yml` runs each target nightly and on
`workflow_dispatch` (`max_total_time`, default 600 s) with `-rss_limit_mb=2048
-malloc_limit_mb=512`, on the nightly toolchain in the `fuzz` stage of `.github/ci/Dockerfile`, and
uploads crash artifacts.

Third-party crate: `tests/fuzz/` depends on `miniz_oxide`, an independent pure-Rust inflate, only as
the output oracle of the `decode` and `zlib_decode` targets (a second implementation to compare
the bytes with, besides the careful path). It is not a dependency of the library.

