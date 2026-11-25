# Performance

## Recommended consumer profile

The library keeps runtime dispatch (AVX2 decode loop; AVX-512 VNNI, AVX2 or NEON Adler-32), so it
runs on any CPU. A binary that runs only on known hosts (for example a Ryzen 7 9800X3D with
avx512f/bw/vl/vbmi) gets more from a whole-program build for that CPU:

```toml
# The binary's Cargo.toml (profiles of dependencies are ignored).
[profile.release]
lto = "fat"
codegen-units = 1
panic = "abort"
```

```sh
RUSTFLAGS="-C target-cpu=x86-64-v4" cargo build --release
```

Caution: a v4 binary dies with SIGILL on an amd64 CPU without AVX-512. Ship a v4 build only to
hosts known to have AVX-512.

`panic = "abort"` is safe with this crate: it does not panic on any input (fuzzed), and it does
not catch panics.

## Reuse: `Decoder` objects

A worker that decodes tile after tile keeps one [`deflate::Decoder`] (or `deflate::zlib::Decoder`)
and one output `Vec`, and clears the `Vec` before each tile:

```rust
let mut d = deflate::zlib::Decoder::new();
let mut out = Vec::new();
for tile in tiles {
    out.clear();
    d.decode_into(tile, &mut out, &limits, &cancel)?;
    // use out
}
```

After the first tile, a decode of a tile of the same size allocates nothing: the `Decoder` holds
the Huffman tables of the last dynamic block, and the `Vec` keeps its capacity.
`tests/reuse_alloc.rs` counts the allocator calls of the second decode of every fixture: 0. The
free functions (`deflate::decode_into`, `zlib::decode_into`) keep one spare set of tables per
thread, so they allocate nothing either after the first call on a thread, but a `Decoder` does not
depend on the thread.

## Build variants in the bench

`.github/workflows/bench.yml` times every fixture with five builds (docs/benchmarks.md has the
numbers):

| Row | Go | Rust |
|---|---|---|
| default | `go build` | `cargo build --release` |
| v4 | `GOAMD64=v4` | `RUSTFLAGS="-C target-cpu=x86-64-v4"` |
| PGO | (default) | `cargo pgo build`, run the bench on the fixtures, `cargo pgo optimize build` |
| PGO + BOLT | (default) | as PGO, then `cargo pgo bolt build --with-pgo`, run, `cargo pgo bolt optimize --with-pgo` |

The PGO and BOLT builds are trained on the same fixtures they are timed on, so their gain is an
upper bound for other data. `tests/parity/pgo.sh` makes them; BOLT is Debian's `bolt-16`
(`llvm-bolt`, `merge-fdata`). Apply PGO and BOLT in the binary that links this crate; the
library itself ships no profile.
