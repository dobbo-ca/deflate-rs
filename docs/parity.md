# Parity with Go

`go/upstream/` is the non-test source of dobbo-ca/deflate-go (with `go.mod` and
`LICENSE`; deflate-go has no NOTICE). `go/oracle/` is a Go program with its own `go.mod` that
`replace`s the module with `../upstream`. The CI job `parity` (`.github/ci/Dockerfile`, stages `go-oracle`,
`go-run`, `parity`) runs:

1. `oracle -gen tests/data/fixtures /inputs` writes the inputs (about 138,000 files).
2. `oracle /inputs manifest.txt` decodes them with deflate-go and writes the manifest (below).
   `go-run` then packs the inputs as `inputs.tar` (and the subset below as `sub.tar`, with its own
   manifest) and deletes the tree in the same `RUN`, so its layers hold two tars and two manifests,
   not a tree of files. The Rust stages untar into `/tmp` and delete it at the end of their `RUN`.
3. `cargo run --release --example oracle -- /tmp/in rust.txt` decodes them with this crate.
4. `tests/parity/compare.sh manifest.txt rust.txt 100000` must pass: at least 100,000 lines, and
   `cmp` equal. On a difference it prints the first 50 differing lines (`<` Go, `>` Rust) and their
   number. A difference is a bug in the Rust port, never in the oracle. Then the same with
   `DEFLATE_SIMD=scalar` on the subset of the `parity-arm64` job (below), with at least 10,000 lines.

Before the manifest the oracles wrote one file per input and operation: some 820,000 files and 11 GB,
which filled the shared BuildKit disk (`no space left on device`) and could not go in the Actions
cache. The manifest holds exactly what was compared then (see "Operations and manifest").

## Inputs

All fixtures (`.zlib` and `.raw.deflate`; the dictionary ones carry `@<dict>` in the name and the
oracle reads `dicts/<dict>.dict`), each with its mutations (`<id>~<tag>`): truncations (every one for a fixture under 4096 bytes, as `x-fx-<id>~t<cut>`; for a larger one Go's 96: the first 32, the last 32 and 32 random cuts, also `x-fx-<id>~t<cut>`), 64 bit
flips at positions from a fixed xorshift seed (the last 32 as `x-fx-<id>~wf<bit>`), 32 byte sets (the last 16 as `~ws`) (0x00, 0xff, a random value), a first block header of every type, edits of the
bytes after it (a stored block's LEN and NLEN, a dynamic block's code lengths), appended bytes,
and for zlib the CMF/FLG bytes (window, FDICT, FCHECK) and the Adler-32. Also:
`compress/flate` and `compress/zlib` output at levels -2..9 for sizes around the window (0, 1,
100, 32767..32769, 70000, and 1 MiB + 13 at levels -2 and 6), the zlib ones also with a 5-byte tail (`gen-*+tail.zlib`); the hand-built streams of
`tests/edge.rs` (`edge-<name>.raw`, written by its `dump_edge_streams` test in the `edge` stage,
so Go supplies the error offsets there); deflate-go's `testdata/fuzz` seeds (`tests/fuzz/seeds`) as
`seed-<name>.raw` and `.zlib`; `dl-` and `dlz-` inputs for `TestAuditDictLong` (below); 300 random raw streams, 300 random streams behind a zlib header,
600 predictor inputs (`.pred`), and the tests of deflate-go's `internal/inflate` below. The `parity-arm64`
job (`.github/ci/Dockerfile`, stage `parity-arm64`, on a native arm64 runner) runs `examples/oracle.rs`
on a subset: every input that is not a mutation (no `~` in its name) and
every 20th mutation; Go decodes the subset again, and the manifests must be equal, once with NEON
and once with `DEFLATE_SIMD=scalar`.

### Ported Go generators

| Go test | Go cases | Generated here |
|---|---|---|
| `TestCoreEdgeTrees` (`trees_core_test.go`): 60 multi-block streams with random complete codes up to 15 bits, empty, degenerate and two-code trees; each decoded, read through 5 readers, and with every `1+len/16`-th bit flipped, under 3 stop configurations of the fast loop | 60 streams, about 128 flips each (about 24,000 checks) | `tree-<i>_<kinds>.raw`: the same 60 streams from `edgeStreams` (a port with the oracle's own xorshift, so the bytes do not depend on the Go version), 40 bit flips each (`~f<bit>`) with every operation, and the rest of Go's flips (every `1+len/16`-th bit) as `x-tree-<i>_<kinds>~f<bit>.raw` (`dec`, `rd`, `rdc`) |
| `TestAuditDictLong` (`audit_test.go`, `zlib/audit_test.go`): for n in 32767, 32768, 32769, 65536, 100000 a stream of `text[n:n+20000]` written with the dictionary `text[:n]`, and a fixed block with matches at distance 32768, read with the right dictionary, `dict[1:]`, one with a changed first byte and `dict[:32767]`; the zlib one for n from 32769, with the right dictionary, `dict[1:]` and none, and cut in the header, the dictionary id and the trailer | 5 sizes x 5 cases; 3 sizes x (3 dictionaries + 10 cuts) | `dl-dl<n>-<case>@dl<n>-<case>.raw` (case `right`, `short`, `head-changed`, `far`, `far-short`; the dictionary in `dicts/dl<n>-<case>.dict`), `dlz-<n>-right@...zlib`, `dlz-<n>-short@...zlib`, `dlz-<n>-nodict.zlib`, `x-dlz-<n>~t<cut>@dl<n>-right.zlib`. The data is the oracle's `genData`, not Go's `input("text")`; the compressors are Go's |
| `TestCoreRandomStreams` (`inflate_core_test.go`): random bytes of length 1..200 with the first block header forced to dynamic | 3,000 | `dyn-<nnnn>.raw`: 3,000 inputs |
| `TestCoreFixedStored`: `fixedStream` (fixed1, fixedMany 5x3000 symbols, fixedNear maxDist 64) and `storedStream` (stored0, storedMix, storedBig 3x65535, storedL0text), each also with trailing bytes | 7 streams, 14 with the tail | `fs-<name>.raw`, `fs-<name>+tail.raw`: 14 inputs (ported generators, own xorshift) |
| `TestCoreTruncations`, `TestCoreBitFlips`: every cut and every bit of `smallStreams` (textL6, textL1, huffOnly, stored, fixed, imageL9) | about 6,000 cuts and 48,000 flips | `x-<name>~t<cut>.raw`, `x-<name>~f<bit>.raw`: all of them, with the operations `dec`, `rd`, `rdc` and `lr1` only (about 216,000 outputs); the streams are made from the oracle's `genData` |
| `limitStreams` (`limit_test.go`, 15; `zlib/limit_test.go`, 13): streams that stop just after n in each way, each with the extra limits 3, 4, 999, 1000, 1001 | 28 streams | `lim-<name>.raw` (15), `lim-z<name>.zlib` (13): 28 inputs (ported generators, own xorshift); the extra limits run as `l3 l4 l999 l1001` |
| `TestCoreZlibBroken` (`zlib/zlib_core_test.go`): every cut and every 3rd bit flip of a zlib stream of `data(3000)`, and an FDICT stream with the id of the empty dictionary (1) | about 1,500 cuts, 4,000 flips, 1 FDICT-empty stream (Go checks it once; the oracle also runs every one of its cuts) | `x-zsmall~t<cut>.zlib`, `x-zsmall~f<bit>.zlib`, `x-zfdict-empty~t<cut>.zlib`: all of them (`dec`, `rd`, `rdc` and `lr1`); the data is the oracle's `genData` |
| `TestCoreZlib`: levels -2..9, sizes 0, 10, 5000, 100000, each also with a 5-byte tail | 48 streams, 96 with the tail | `gen-l<level>-<n>.zlib` and `gen-l<level>-<n>+tail.zlib` (sizes around the window rather than Go's four; the level range is Go's) |
| `TestEquality`, `TestDecodeLimit` (top level): `Decode` against stdlib on the fixtures and on `mutations()`; a random limit n per mutation | per mutation: every cut under 4096 bytes else 96, 64 flips, 32 byte sets, one random n | the same inputs (`fx-<id>~...`, `x-fx-<id>~t<cut>`; the 96 cuts of a larger fixture are the head 0..31, the tail n-1..n-32 and 32 random ones from the oracle's xorshift, not Go's PCG, so the random positions differ); `lr1 lr2 lr3` are three random n for one mutation in eight, and `lr1` alone for every `x-` mutation (FNV-1a 64 of the name and k, modulo the output length + 1, the same in both oracles; Go's 32 KiB-multiple choice of n is not ported). Go supplies the expected bytes instead of stdlib |
| `TestAuditEOBLongTruncations` (`internal/inflate/audit_test.go`): 16 streams of random trees with EOB the longest code, every cut, under 4 stop configurations of the fast loop | 16 streams, about 1,500 cuts each | `x-eoblong<i>~t<cut>.raw`: every cut (`dec` and `rd`) |
| `TestTableSymMatchesStdlib`: 2,000 trees (random complete, empty, degenerate, junk code lengths) checked at table level | 2,000 trees | `junk-<nnn>.raw`: 400 dynamic blocks (a litlen and a distance tree each, of those kinds) decoded as streams |

Go supplies the expected bytes and error offsets for all of them; the Rust side is not told. The
`x-` inputs run only `dec`, `rd`, `rdc`, `par` and `lr1` (not the 12 or more `DecodeLimit` and chunking operations
of the others), to bound the run time. Not generated: the forced stops of the fast loop (`withStops` in `TestCoreHandoff` and
`TestAuditEOBLongTruncations`: an internal knob of the Go loop; the Rust fast loop is compared with
the careful path in `tests/fast.rs`, and with Go on the same streams and cuts here, which `TestCoreHandoff` uses as
inputs: the cuts and flips of `smallStreams` are the `x-` inputs), the `Close` check of
`compareRC` (this crate has no `Close`), the 2,000 trees of `TestTableSymMatchesStdlib` (400 are run
as stream blocks) and `cmd/tiles`. The consumed input of `compareRC` is compared: see `rdc`.

## Go tests, one by one

Every test of deflate-go is ported (a Rust test in `tests/`), covered by the oracle
inputs above, or not applicable, with the reason.

| Go test | Here |
|---|---|
| `TestEquality` (root, zlib), `TestDecodeLimit` (root, zlib) | covered: fixtures and their mutations, `dec` and the `l*` operations (the `x-` mutations get `lr1` only); `tests/fixtures.rs` (`decode_limit_is_a_prefix`, `bit_flips_agree`, `truncations_are_unexpected_eof`) |
| `TestLimitStreams` (root, zlib) | covered: the `lim-` inputs |
| `TestEdgeStreams`, `TestAuditEOBLong` | ported: `tests/edge.rs`; covered: `edge-<name>.raw`, `x-eoblong<i>~t<cut>.raw` |
| `TestDecoderReuse` (root, zlib) | ported: `tests/reuse.rs` (large and small streams alternate through `decode_into` with one cleared `Vec`; each output is compared with a fresh decode) |
| `TestCorpus` (root, zlib) | ported: `tests/corpus.rs` (`DEFLATE_STREAMS`, skipped when unset, as Go's) |
| `TestCoreAPI` | ported: `tests/partial.rs` (`success_appends`, `earlier_bytes_are_not_history`) and the fixtures |
| `TestReader`, `TestReaderDict` (root, zlib) | covered: `rd` and `rdc` on every input and `c<p>` on the non-mutations and one mutation in eight, never on `x-` inputs; the `zdict*` fixtures get their right dictionary only (Go also reads each with a wrong, an empty and half of the right one; the short, head-changed and no-dictionary cases are the `dl-`/`dlz-` inputs of `TestAuditDictLong` only); `tests/reader.rs`, `tests/fixtures.rs` (`readers_equal_decode`) |
| `TestReaderStoredFillsWindow` | ported: `tests/reader.rs` (`stored_blocks_across_the_window`) |
| `TestAuditStoredAcrossFills` | covered: `rd` and `c<p>` on `fs-storedBig`, `fs-storedMix`, `fs-storedL0text`; `tests/reader.rs` (`reads_end_at_each_32k`) |
| `TestReaderSyncFlush` | ported: `tests/reader.rs` (`reads_end_at_a_sync_flush`) |
| `TestAuditOddReaders` | ported: `tests/reader.rs` (`odd_readers`) |
| `TestAuditDictLong` (root, zlib) | covered: `dl-`, `dlz-` inputs (wrong, short, head-changed, 32767-byte dictionaries at the 32767/32768 boundary); `tests/reader.rs` (`dictionary_is_the_history`) |
| `TestAuditExactCapTruncatedStored` (root, zlib) | ported: `tests/zlib.rs` (`exactcap_truncated_stored`), `tests/partial.rs`; the `exactcap-stored` seed is an input |
| `TestAuditTrailerErrors` | ported: `tests/zlib.rs` (`trailer_errors`), `tests/reader.rs` (`errors_from_the_input_pass_through`) |
| `TestCoreZlib`, `TestCoreZlibBroken`, `TestCoreZlibDict` | covered: `gen-l*`, `x-zsmall*`, `x-zfdict-empty*`, the `zdict*` fixtures; `tests/zlib.rs` |
| `TestCoreLevels`, `TestCoreFixedStored`, `TestCoreTruncations`, `TestCoreBitFlips`, `TestCoreRandomStreams`, `TestCoreEdgeTrees`, `TestCoreDict` | covered: `gen-l*`, `fs-*`, `x-<name>~t/~f*`, `dyn-*`, `tree-*` and `x-tree-*`, the dictionary fixtures (see the table above) |
| `TestCoreHandoff` | covered without the forced stops of the fast loop: the cuts and flips of `smallStreams`; `tests/fast.rs` compares the fast loop with the careful path |
| `TestAuditEOBLongTruncations` | covered without the forced stops: `x-eoblong<i>~t<cut>.raw` |
| `TestTableSymMatchesStdlib` | partly: 400 of the 2,000 trees run as dynamic blocks (`junk-*`); the table-level check is not ported (Go's table layout is not this crate's) |
| `TestDecodeContextCancelMidStream` | replaced: `tests/limits.rs` (the cancel flag is a parameter, there is no `context`) |
| `TestAuditResetClose`, `TestAuditResetOrder`, `TestReaderReset` (root, zlib), `TestCoreReset`, `TestCoreZlibReset` | not applicable: this crate has no `Reset` or `Close`. What `TestAuditResetClose` also checks, reads after the end or an error and `Read(nil)`, is `eof_and_errors_repeat` and `zero_length_read` in `tests/reader.rs` and the fuzz target `reader`. The empty read differs from Go in one way: Go runs a decode step |
| `TestAuditByteReaderOverride` (root, zlib) | not applicable: Go picks its own `bufio` or the source's `ReadByte`; the readers here take a `BufRead` and consume only what they decode (`rdc`, `tests/reader.rs` `stops_at_the_end_of_the_stream`) |
| `TestAuditBufioUnreadByte` | not applicable: there is no `UnreadByte`; the bytes after the stream are compared through `rdc` on the `+tail` and `fs-*+tail` inputs |
| `TestAuditDecodeSpareCap` | not applicable: a `Vec` has no spare-capacity contract; `decode_into` appends and the `decode` fuzz target checks the capacity against `max_alloc` |
| `TestAuditDecodeAlias` (root, zlib) | not applicable: Rust does not allow `dst` to alias `src` |
| `TestDecodeContextEqualsDecodeLimit` | not applicable: there is no separate context entry point, the cancel flag is a parameter of `decode_limit` |
| `TestDecodeLimitAllocs` (root, zlib), `TestCoreZeroAllocs`, `TestCoreZlibZeroAllocs`, `TestDecodeLimitCapBounded` | not applicable: Go allocation counts; the Rust counterpart is the `max_alloc` charge (`tests/limits.rs`, the fuzz targets) |
| `TestPairTable`, `TestFuseAdler`, `TestFusedAdler` | not applicable: Go's table and fused-checksum internals; the Rust decoder and checksum are checked through the output (`tests/zlib.rs`, `tests/fast.rs`, parity) |
| `TestFastStateOffsets`, `TestFastInitPick`, `TestFastAsm*`, `TestPageGuardFaults`, `TestFastAsmPageGuard*` (amd64), `TestAsm*` (arm64) | not applicable: Go's assembly loop (struct offsets, page guards, canaries); the Rust fast loop is compared with the careful path (`tests/fast.rs`) and with Go (parity, both architectures) |
| `TestLengthsOffsets`, `TestBlockEdges`, `TestFill`, `TestChained`, `TestRandom` (adler32) | ported: `tests/adler32.rs`, `src/adler32` unit tests, the `adler32` fuzz target |
| `TestModMul` | not applicable: a helper of Go's amd64 assembly |
| `TestRGBA`, `TestUndoHorizontal`, `TestWrap`, `TestPanics` (predictor) | ported: `tests/predictor.rs`; covered: the `p-*.pred` inputs |
| `TestAllocs`, `TestGuard`, `TestSIMDMatchesGo` (predictor), `TestCorpus` (predictor) | not applicable: allocation counts, assembly page guards and SIMD against generic code (the Rust predictor is scalar), and a corpus of tiles that is not in the repo |
| `Fuzz*` functions (root, zlib, `FuzzReaderWindow`) | their seeds are `tests/fuzz/seeds` and parity inputs; the `reader`, `decode`, `zlib_decode` and `decode_limit` targets of `tests/fuzz/` replace them |
| `TestTiles` (`cmd/tiles`) | not applicable: a TIFF tile extractor for the corpus tests, not part of the library |

## Operations and manifest

Per stream the oracle runs `dec` (`Decode`), `rd` (a reader over the bytes, read to the end, with
the stream's dictionary), `par` (`decode_parallel_chunked_into` with 4 threads and 1 KiB chunks, against Go's `Decode`), `rdc` (the input bytes that reader consumed, as the line `consumed <n>`, whatever the outcome), `l0 l1 l2 l100 l1000 l32767 l32768 l32769 l65535 l65536 l65537 l70000` (`DecodeLimit` at that n), `lh` and `lp` (`DecodeLimit` at half, and 1000 more than, the
bytes `Decode` returned, also when it failed), `lr1 lr2 lr3` (`DecodeLimit` at a random n, on the mutations that `c1`.. also run on; the `x-` inputs run `lr1` only), `lq-1 lq+0 lq+1` (at the length of the partial output, +/-1, of a stream that fails), `c1 c100 c4096 c65536` (a reader read with
that buffer size, as `readChunks` of `reader_test.go`: the FNV-1a 64 and length of the bytes, then the sizes of its non-empty reads as `size*count` lines, which end at each 32 KiB of the history and at each sync flush as Go's, then the `consumed <n>` line, and the error; the manifest line hashes this whole text; on every input that is not a mutation and one mutation in eight, by the sum of the bytes of its name), and, for a
stream that decodes, `lo-1 lo+0 lo+1` (`DecodeLimit` at the output length -1, 0 and +1) and `lN`
at k-1, k and k+1 for every multiple k of 32 KiB from 64 KiB that is within 32 KiB of the output
end or past it (up to the output length + 32 KiB). An input whose name starts with `x-` (the exhaustive cuts and flips) runs only `dec`, `rd`, `rdc`, `par` and `lr1` (one `DecodeLimit` at a random n, so a limit meets every cut and flip). Per
predictor input it runs `run`.

Both oracles write one manifest file: the inputs in byte order of their names (Go's `os.ReadDir`
order; the Rust side sorts), and per input its operations, one line each (the two oracles run them in a different order; `compare.sh` sorts both manifests):

- `<input>.<op> out <len> <fnv64a-hex>`: the output bytes (their length and FNV-1a 64 as 16
  lowercase hex digits), or
- `<input>.<op> err <class>`: the error class, followed, if bytes came back with the error (Go
  returns them and so does this crate: `decode_into`, `decode_limit_into`, the readers), by
  `<input>.<op> partial <len> <fnv64a-hex>` for them (a predictor input has none).

The two manifests must be equal. A 64-bit hash stands for the bytes: a difference that keeps the
length and the hash is not found by the check (about 2^-64 per output).

A predictor input is `[op][spp][flag][w u32 LE][h u32 LE][src]`. `op` 0 is `undo_expand_rgba`,
1 is `expand_rgba` (`flag` is `invert_grey`; the output is `dst`, `4*w*h` zero bytes, or none when
`w*h` passes 1<<20), 2 is `undo_horizontal` (`flag` is `bytes_per_sample`; the output is `src`).

## Error classes

| class | Go | Rust |
|---|---|---|
| `corrupt@<off>` | `flate.CorruptInputError(off)` | `Error::Corrupt(off)` |
| `unexpected-eof` | `io.ErrUnexpectedEOF` | `Error::UnexpectedEof` |
| `internal` | `flate.InternalError` | `Error::Internal` |
| `header` | `zlib.ErrHeader` | `Error::Header` |
| `checksum` | `zlib.ErrChecksum` | `Error::Checksum` |
| `dictionary` | `zlib.ErrDictionary` | `Error::Dictionary` |
| `invalid-argument` | a panic of `predictor` | `Error::InvalidArgument` |
| `bad-input` | a `.pred` file that is short or has an unknown op | the same |

`Error::OutputLimit`, `AllocLimit`, `OutOfMemory`, `Cancelled` and `Io` have no Go counterpart
(Go has no limits); they would print as `other:<message>` and show up as a diff.


## How parity is checked, and what tests/ covers


The decoder is a port of the careful path of Go's `compress/flate` `inflate.go`: one input byte
at a time, the same bit buffer, the same table lookups. So every error and its offset, and
the bytes a reader consumes, come out as in Go. `decode_limit` ports the rules of deflate-go's
`DecodeLimit` (design.md 3.1): it stops where a `LimitReader` over Go's reader stops, including
the 32 KiB window flush.

Every Go test is ported (`tests/`), covered by the oracle inputs, or not applicable here;
the section "Go tests, one by one" above lists each with its counterpart or the reason.
`TestDecoderReuse` is `tests/reuse.rs`: large and small streams alternate through `decode_into`
with one cleared `Vec`, and each output equals a fresh decode. Not applicable: the reuse and `Close`
tests of readers (`TestReaderReset`, `TestCoreReset`, `TestCoreZlibReset`, `TestAuditResetClose`,
`TestAuditResetOrder`: this crate has no `Reset` or `Close`; its decode functions keep no state
between calls, only the thread's spare Huffman tables, which the `decode` fuzz target checks against
a fresh thread), `TestAuditByteReaderOverride` and `TestAuditBufioUnreadByte` (Go's `ByteReader` and
`bufio` handling: the readers here take a `BufRead` and consume only what they decode, which the
oracle compares as `rdc`), `TestAuditDecodeSpareCap` and `TestAuditDecodeAlias` (a `Vec` has no
spare-capacity contract and `dst` cannot alias `src`), `TestDecodeContextEqualsDecodeLimit` (there is
no context entry point: the cancel flag is a parameter, and `TestDecodeContextCancelMidStream` is
`tests/limits.rs`), the tests of Go's assembly loops, `cmd/tiles` and its `TestTiles` (a TIFF tile
extractor for the corpus test, not part of the library). Without the forced stops of the fast loop
(`withStops` in `TestCoreHandoff` and `TestAuditEOBLongTruncations`: there is no stop hook;
`tests/fast.rs` compares the fast loop with the careful path at its natural stops) and at table
level for `TestTableSymMatchesStdlib` (400 of its 2,000 trees of random, empty, degenerate and junk
code lengths are generated as dynamic blocks and run through the decoders). `fixedStream` and
`storedStream` (`TestCoreFixedStored`), the exact truncations and bit flips of `smallStreams`
(`TestCoreTruncations`, `TestCoreBitFlips`) and the preset-dictionary cases of `TestAuditDictLong`
(wrong, short, head-changed and 32767-byte dictionaries at the 32767/32768 boundary) are generated;
Go's `mutations` of the fixtures are matched (every truncation of a fixture under 4096 bytes, and for
the larger ones Go's 32 cuts at the head, 32 at the tail and 32 random ones, as `x-fx-` inputs; 64
flips and 32 byte sets of each, half of them as `x-` inputs; the oracle's limits have 2, 100, `len/2`,
`len+1000` and three random `lr<k>` too), and so are the flips of `TestCoreEdgeTrees` at Go's density
(`x-tree-` inputs). The oracles also compare the input bytes a reader consumed (`rdc`, and a line in
`c<p>`), error paths included. Both oracles write one manifest (a line per input and operation, with
the length and FNV-1a 64 of the output), not a file per output. The `parity` job also runs the oracle
with `DEFLATE_SIMD=scalar`, on the subset also used by `parity-arm64`, and `compare.sh` fails on
fewer than 100,000 manifest lines of the full run and 10,000 of the subset runs, on a failed sort or
on a different line count. The `parity-arm64` job
runs the oracle on a native arm64 runner and compares its manifest with Go, on a subset of
the inputs ("Inputs" above), once per SIMD tier.

`tests/parity/` has Go's sources at the pinned sha and a small Go program, run in CI over the same
inputs as `examples/oracle.rs`, with the output or the error class of each input diffed
(this page). `tests/` checks what needs no Go (the edge streams are also run through Go, see "Inputs" above):

- `edge.rs`: hand-built streams (degenerate and deep trees, every length and distance code,
  overlapping copies, stored blocks, every bit alignment) with the output they must give, and
  the Go error and offset for the invalid ones, worked out from the stream.
- `fixtures.rs`: the deflate-go fixtures (`tests/data/fixtures`, from zlib 1.2.11 at every level,
  strategy, wbits and memLevel, with flushes, dictionaries, PNG and TIFF tiles). All streams of one
  input must give the same bytes of the manifest's size, and the zlib ones check their own
  adler32. Every truncation is an unexpected EOF; flipped bits give equal results through the
  whole-buffer and streaming APIs.
- `reuse.rs`: Go's `TestDecoderReuse`, one cleared `Vec` through large and small streams.
- `limits.rs`, `reader.rs`, `zlib.rs`, `predictor.rs`, `adler32.rs`: limits and cancellation,
  the reader window and dictionary, headers and trailers, the predictor against a per-pixel
  reference, adler32 against its definition.
- `corpus.rs`: `DEFLATE_STREAMS=<file>` runs records of `[u32 LE length][zlib stream]` through
  every API.

CI (`.github/workflows/ci.yml`, `.github/ci/Dockerfile`) runs fmt, clippy `-D warnings`, the tests,
the tests with `DEFLATE_SIMD=scalar`, and an aarch64 release build in `test`; the `arm64`
job runs the tests with both tiers on a native arm64 runner; the `parity` job compares the manifest of
the Go oracle with that of `examples/oracle.rs`, and `parity-arm64` does the same on the arm64
runner. The jobs build the Dockerfile stages with BuildKit on GitHub-hosted runners.

## Real-file check

`oracle -qdecode` (`tests/parity/go/oracle/quality.go`) and `examples/quality.rs` decode a
directory of `<stem>.w<W>.c<C>.p<P>.zlib` inputs to PGM/PPM, Go and Rust. The two output
directories must be byte-identical. `oracle -qgen` builds such inputs from TIFF files (for example
the OpenSlide test data) and the fixtures. This is a manual check, not a CI job.
