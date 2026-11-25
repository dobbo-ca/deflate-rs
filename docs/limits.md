# Limits and resource control

## Resource limits

A call stays inside the limits you give it, so a caller in a small pod can bound it:

```rust
use deflate::Limits;
use std::sync::atomic::AtomicBool;

let limits = Limits { max_output: 32 << 20, max_alloc: 64 << 20 };
let cancel = AtomicBool::new(false); // set it from another thread to stop the call
match deflate::zlib::decode(tile, &limits, &cancel) {
    Ok(out) => { /* at most 32 MiB */ }
    Err(deflate::Error::OutputLimit) => { /* more than max_output */ }
    Err(deflate::Error::AllocLimit) => { /* working memory would pass max_alloc */ }
    Err(deflate::Error::Cancelled) => { /* the flag was set */ }
    Err(e) => { /* a bad stream, or the allocator refused */ }
}
```

| `Limits` field | Default | Unit | Error when passed |
|---|---|---|---|
| `max_output` | 1 GiB (`1 << 30`) | bytes of output (the predictor: bytes processed) | `Error::OutputLimit` |
| `max_alloc` | 1 GiB (`1 << 30`) | bytes of working memory | `Error::AllocLimit` |

- `max_alloc` is the enforced peak of the whole call. Every allocation that grows with the input
  (the output `Vec`, a reader's window, a preset dictionary) is charged when it is allocated and
  released when the call returns or the reader is dropped. It is not an estimate made up front.
  The fixed tables (under 64 KiB) are not charged. `tests/alloc_peak.rs` measures the peak with a
  counting allocator. An allocator that refuses a size within the limits is `Error::OutOfMemory`.
  One exception to the peak: while the output `Vec` grows, a `realloc` that cannot extend the block
  in place briefly holds the old block and the new one (up to about three times the output so far, at most about twice
  `max_alloc`; allocator-dependent, glibc moves large blocks with `mremap`). That moment is not charged.
- Cancellation: pass an `&AtomicBool`; when it is set the call returns `Error::Cancelled` (a
  reader's `io::Error` has kind `Other`). It is checked at each DEFLATE block and every 64 KiB of
  output, and by the predictor per row.

## Limits

Every entry point takes `Limits { max_output, max_alloc }`. Go had none, so the defaults are 1 GiB
each. Output past `max_output` is `Error::OutputLimit`. The output buffer (or a reader's window)
is charged against `max_alloc` before it is allocated, never from a size in the input, and
growth is `try_reserve_exact`: `Error::AllocLimit` or `Error::OutOfMemory`, not an abort. A
reader needs `max_alloc` of about 100 KiB. The fixed tables (under 64 KiB) are not charged.

Every decode, readers included, checks its cancel flag at each DEFLATE block, and inside a Huffman
block every 64 KiB of output, and returns `Error::Cancelled` (a reader's `io::Error` has kind
`Other`, as the error is final). A stream of empty blocks makes no output, so a reader needs the
flag too. A reader's preset dictionary is charged against `max_alloc`. The predictor
allocates nothing: it writes into the caller's buffers. It takes `Limits` and a cancel flag too:
the bytes it processes (`4 * w * h` for the expands, `w * h * spp * bytes_per_sample` for
`undo_horizontal`) are checked against `max_output` (`Error::OutputLimit`) and the flag is checked
per row and every 64 KiB of one (`Error::Cancelled`). A short slice, an unsupported `spp` or an overflowing size is
`Error::InvalidArgument` where Go panics. `adler32::update` (`#[doc(hidden)]`, for tests and the fuzz target) takes `Limits` too: a slice
longer than `max_output` is `Error::OutputLimit`; the crate's own calls take the unchecked `sum`, on bytes a decode has charged already.

