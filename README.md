# deflate-rs

DEFLATE (RFC 1951) and zlib (RFC 1950) decoding, TIFF predictor 2 and RGBA expand, in Rust.
A port of [dobbo-ca/deflate-go](https://github.com/dobbo-ca/deflate-go) (vendored in `tests/parity/go/upstream`). For every
input the output bytes of a decode that succeeds, and the error of one that fails, equal Go 1.27
`compress/flate` and `compress/zlib`. Decode only. Pure Rust, std only; `unsafe` only in the
kernel modules.

```toml
deflate = { git = "https://github.com/dobbo-ca/deflate-rs", tag = "v0.1.0" }
```

## Quick start

```rust
use deflate::Limits;
use std::sync::atomic::AtomicBool;

// Explicit limits: at most 64 MiB of output, and 64 MiB of working memory for the whole call.
let limits = Limits { max_output: 64 << 20, max_alloc: 64 << 20 };
let cancel = AtomicBool::new(false);

let out = deflate::zlib::decode(tile, &limits, &cancel)?;   // zlib
let out = deflate::decode(raw, &limits, &cancel)?;          // raw DEFLATE
// At most n bytes, as io.ReadAll(io.LimitReader(r, n)): a stop at n skips the adler32.
let head = deflate::zlib::decode_limit(tile, n, &limits, &cancel)?;

// Go returns the output with an error (io.ReadAll). The *_into functions append to a Vec and
// leave the bytes before the error in it, also for decode_limit_into and zlib::decode_into.
let mut out = Vec::new();
let r = deflate::decode_into(raw, &mut out, &limits, &cancel);

// Streaming: std::io::Read over a BufRead, with an optional preset dictionary.
let mut r = deflate::zlib::Reader::with_dict(std::io::BufReader::new(file), dict, &limits, &cancel)?;

// Reuse the reader's buffer for the next stream: no allocation if it needs no more memory.
let buf = r.into_buffer();
let mut r2 = deflate::zlib::Reader::with_buffer(std::io::BufReader::new(file2), buf, &limits, &cancel)?;
```

## Resource limits

| `Limits` field | Default | Unit | Error when passed |
|---|---|---|---|
| `max_output` | 1 GiB (`1 << 30`) | bytes of output | `Error::OutputLimit` |
| `max_alloc` | 1 GiB (`1 << 30`) | bytes of working memory, the enforced peak of the call | `Error::AllocLimit` |

Cancellation: set the `&AtomicBool` and the call returns `Error::Cancelled`. `Limits` has no
`max_threads` field (the documented exception, so that tiff-rs's `Limits { max_output, max_alloc
}` literal compiles unchanged): every call runs on the calling thread, except
`decode_parallel_into` and `zlib::decode_parallel_into`, which take `max_threads` as an argument
(`1` spawns no thread; no pool) for one large stream, with the serial output and errors. See
[docs/limits.md](docs/limits.md) and [docs/threading.md](docs/threading.md).

## Docs

- [docs/limits.md](docs/limits.md): limits, the `max_alloc` charge, cancellation, errors
- [docs/threading.md](docs/threading.md): the parallel decode of one stream
- [docs/safety.md](docs/safety.md): `unsafe`, SIMD tiers, the fast loop, Miri
- [docs/parity.md](docs/parity.md): the Go oracle, inputs, Go tests one by one
- [docs/fuzzing.md](docs/fuzzing.md): fuzz targets, seeds, `miniz_oxide`
- [docs/benchmarks.md](docs/benchmarks.md): Go against Rust timings
- [docs/performance.md](docs/performance.md): consumer build profile, `Decoder` reuse, PGO and BOLT
- [docs/go-differences.md](docs/go-differences.md): API and behaviour differences, not ported

## Licence

Apache-2.0, see LICENSE. NOTICE has the Go standard library licence for the ported code.
