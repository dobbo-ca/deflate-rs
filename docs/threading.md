# Threading

From the "Resource limits" section of [limits.md](limits.md).

- Every function runs on the calling thread only, except `decode_parallel_into` and
  `zlib::decode_parallel_into`. `Limits` has no `max_threads` field, on purpose: it is kept
  so that tiff-rs, which builds
  the `Limits { max_output, max_alloc }` struct literal, compiles unchanged. The two parallel
  functions take `max_threads` as an argument instead. `tests/threads.rs` checks that the other
  functions do not grow the process thread count.
- Run independent streams or tiles on your own threads: that is the best use of the cores when
  there are many tiles. The parallel functions are for one large stream.

## Parallel decode of one stream

`decode_parallel_into(src, out, limits, max_threads, cancel)` (and the zlib one) decode one large
stream with up to `max_threads` threads, the calling thread included. The output, and any error
with its partial output, are byte for byte those of `decode_into` (for `Error::Cancelled`, `out`
holds no new bytes).

```text
 src   |---- chunk 0 ----|---- chunk 1 ----|---- chunk 2 ----|   1 MiB or more each
        s0=0              s1 (found)        s2 (found)
 find   -                 search <=256 KiB  search <=256 KiB      in parallel
 decode [s0, s1) u16       [s1, s2) u16      [s2, end) u16         in parallel; markers for
                                                                  the 32 KiB before a span
 join   windows in order (32 KiB per span), then u16 -> bytes     in parallel
```

1. Find: worker `k >= 1` looks, in the first 256 KiB of its chunk, for a bit position where a
   cheap test on the next 120 bits passes (block type, HLIT, HDIST, a complete code length code),
   then a non-final dynamic block header parses, its codes are what Go accepts, and the whole block
   decodes. That is a guess: a worker with no guess leaves its chunk to the span before it.
2. Decode: each span decodes from its guess into 16-bit symbols: a byte, or a marker for a byte
   of the 32 KiB before the span, unknown until the spans before it are decoded. It must end a
   block exactly at the next span's guess, and the last span must end the final block.
3. Join: the window of each span from the ones before it, in order; then every span turns into
   bytes in its slice of the output, in parallel.

Exact output: span 0 starts at the real start. A later span counts only if the span before it,
decoded from a real block start, ended a block exactly at its guess; so every guess that counts
is a real block start. Every check the serial decoder makes (header fields, code completeness
with its own `accept_lengths`, symbols 286 and 287, distances 30 and 31 and past the start of
the history, stored LEN and NLEN, the zlib Adler-32, the output and alloc limits) gives up here
instead; then the serial decoder decodes the whole stream, so an error, its offset and the
partial output are the serial ones. A give-up costs the time spent, never a different result.

Limits and costs:

- `max_threads`: the peak of threads, the calling thread included. With fewer than 4 workers
  (1 MiB of input each, so below 4 MiB, or `max_threads` under 4) the decode is serial and
  nothing is spawned: a span decodes into u16 symbols several times slower than the serial loop,
  so 2 or 3 workers do not pay for the search and the join. Threads are spawned per call with `std::thread::scope`,
  one scope per pass (find, decode, join; the next pass needs all results of the one before, and
  the output size is known only after the decode): no pool, no state between calls, never more
  than `max_threads` alive. A thread the OS refuses is done on the calling thread (find, decode)
  or makes the call fall back to serial (join). The spawn cost (tens of microseconds) is small
  next to a stream of megabytes.
- `max_alloc`: the peak of all threads together, charged before each allocation: each worker's
  tables (about 35 KB), the 16-bit spans (two bytes per output byte, grown by doubling), then the
  output while they are held, then the 32 KiB windows of the join: about 4 to 5 times the output
  (4.1 times with 4 threads and 4.8 with 8 on a 768 KiB noise image). If that does not fit, the
  decode falls back to the serial one, which needs only the output.
- A hostile input costs each worker at most its 256 KiB search and 16 Mi trial symbols (the
  trial decodes of the guessed blocks), not a decode per guess.
- `cancel` is checked at each block and every 64 Ki symbols inside one, in every worker and in the
  search's trial decodes (a cancel ends the search).

Measured (amd64, Ryzen 7 9800X3D, 16 CPUs): on the bench's 64 MiB stream of the fixtures' outputs (text,
image, runs; zlib level 6) the parallel decode takes 24 ms where Go takes 47 ms and the serial
decode 47 ms (0.52). On a 33 MB stream of real slide pixels (OpenSlide CMU-1 tiles, `oracle -qgen`) it
takes 22 ms where the serial decode takes 17.5 ms: a span decodes into u16 symbols several
times slower than the serial loop on literal-heavy image data, and the u16 buffers add page
faults. So use it for one large stream of text-like data; for tiles, decode the tiles in
parallel instead (each tile serially). A faster span decoder is the open item.

Tests: `tests/parallel.rs` (every fixture split into chunks of 64 bytes to 4 KiB, 2 to 8 threads,
with a prefix, mutated, and at tight limits, against the serial decode), `tests/parallel_threads.rs`
(the peak of spawned workers, counted by the crate, against `max_threads`), the parity op `par` (every parity input, 4
threads, 1 KiB chunks, against Go's `Decode`), and the `parallel` fuzz target.
