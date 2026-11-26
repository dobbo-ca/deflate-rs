# Safety, unsafe code and kernels

## SIMD and kernels

The crate root denies `unsafe_code`; the kernel modules below allow it, under this
rule: `unsafe` is allowed in kernels to make the crate as
fast as possible, every input and output is checked at the kernel boundary in safe code, every
`unsafe` block has a `// SAFETY:` comment, and each kernel has a scalar or safe reference that
the tests and a differential fuzz target compare it with.

- adler32 (`src/adler32/`) has SIMD tiers: AVX-512 VNNI and AVX2 (x86_64) and NEON (aarch64),
  picked once at first use by runtime detection; `DEFLATE_SIMD=scalar` forces the scalar code,
  which is always compiled and is the oracle. Each tier has two kernels: the sum, and a copy that
  sums what it copies in the same pass (a zlib stored block is copied and summed at once). The
  kernels are safe `#[target_feature]` fns that take arrays (`&[u8; N]`, from `as_chunks` in
  safe code); `unsafe` is only in each tier's two calls into them, next to its detection, in the
  tier's `mem` module (one load and one store that take `&[u8; N]` / `&mut [MaybeUninit<u8>; N]`),
  and in the `set_len` after a copy into spare capacity. The unit tests in `src/adler32/mod.rs`
  compare each kernel with scalar bit for bit (xorshift and all-0xff data, edge lengths around
  each lane width, unaligned sources and destinations).
- The decode loop (`src/fast/kernel.rs`) writes straight into the spare capacity of the output
  `Vec` through a raw pointer, as deflate-go's assembly does: no zero-fill, no bounds check per
  store, `set_len` at the end. Its entry checks, in safe code, the bounds that every access relies
  on (the capacity holds `OUT_MARGIN` past where a pass may start, the history start is below the
  output position); the module doc gives the invariants. The careful path alone
  (`decode_careful_into`) is its safe reference: `tests/fast.rs` compares the two.
- On x86_64 with AVX2, BMI1 and BMI2 the same loop is also compiled inside a safe
  `#[target_feature(enable = "avx2,bmi1,bmi2")]` fn (`src/fast/avx2.rs`), so the shifts and masks
  become SHRX, SHLX and BZHI, as Go's BMI2 tier (`fast_amd64.go`), and a 32-byte match copy one
  load and one store. That instantiation (`AVX2 = true` in `run_impl`) also copies a short-period
  match (distance under 16) with PSHUFB and 32-byte stores; those intrinsics sit in `unsafe`
  blocks in the kernel, whose SAFETY comments name the detection. It is a kernel module under
  the same rule; `DEFLATE_SIMD=scalar` turns it off. A unit test compares it with the generic
  loop, bit-exact, and the fuzz targets run it on x86_64.

`deflate::tiers()` (hidden) names the tiers dispatch picked; `tests/fast.rs` asserts VNNI (when
the CPU has it, else AVX2) and the AVX2 loop on x86_64, NEON on aarch64, and scalar and generic under
`DEFLATE_SIMD=scalar`. The CI `arm64` job runs the tests on a native arm64 runner, once per
tier.
The `miri` job runs the non-SIMD unsafe (the loop, the copy into spare capacity) under Miri
through the small `miri_` unit tests, where dispatch picks the generic loop and scalar adler32.
The `kernels` fuzz target compares the loop with the careful path and every copy kernel with a
copy and the definition; it runs on x86_64 and, in `fuzz-arm64`, built for aarch64 (NEON).


## Fast loop

`src/fast.rs` (the tables) and `src/fast/kernel.rs` (the loop; its AVX2 build is `src/fast/avx2.rs`)
port deflate-go's fast loop and tables (`fast_generic.go`, `tables.go`): a 64-bit
bit buffer refilled by an 8-byte load, a litlen table of 11 bits and a distance table of 8 bits
with subtables for longer codes, up to three literals per refill, wide copies for matches, and a
pair table that decodes two short literals per lookup when a model says the block pays for it.
The loop only runs between symbols of the careful path, and stops before anything it does not
handle (end of block, a bad code, a distance past the history, a missing code, the last bytes of
input or output, a limit), with no side effect for that symbol; the careful path decodes it and
reports any error. On the way out the loop hands back the bit buffer and the number of bytes
Go's reader would have pulled (as deflate-go's fast loop does), so errors, offsets and reads equal Go's.
`tests/fast.rs` compares it with the careful path alone (`decode_careful_into`, hidden) on the
fixtures, mutations, random streams and matches at every length and distance across its margins;
the `parity` job compares both with Go. It writes into the spare capacity of the output `Vec`
(see "SIMD and kernels" above). In a zlib decode the Adler-32 is summed after each call of the loop and
as stored blocks are copied, while the bytes are in cache, as Go does.

