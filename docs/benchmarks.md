# Benchmarks

## Hosted runner baseline (2026-11-25)

The baseline for `bench.yml` on a GitHub-hosted `ubuntu-24.04` runner (bench run 45701436864,
AMD EPYC 9V74, 2 vCPUs, avx2 bmi2 avx512f/bw/vl/vnni). Rust / Go time, Go default GOMAXPROCS. A
shared hosted VM is noisier than the dedicated node of the sections below, so do not compare
these numbers with them as a regression.

| class | default | v4 | PGO | PGO + BOLT |
|---|---|---|---|---|
| text | 0.81 | 0.82 | 0.80 | 0.80 |
| image | 0.82 | 0.83 | 0.83 | 0.83 |
| random | 5.22 | 5.24 | 5.23 | 5.22 |
| runs | 0.91 | 0.87 | 0.93 | 0.92 |
| zeros | 0.92 | 0.90 | 0.94 | 0.93 |
| png | 0.99 | 0.97 | 1.02 | 1.01 |
| tiff | 1.09 | 1.09 | 0.98 | 0.99 |
| all | 0.93 | 0.93 | 0.89 | 0.90 |

The v4 build shows no gain over the default build here, so no v4 flags are recommended for this
crate. The one large stream (67.1 MB out) decodes in 65.6 ms against Go's 64.0 ms (1.02): with
2 vCPUs the parallel decode falls back to the serial one (it needs 4 workers). On a runner without
AVX-512, `tests/parity/bench.sh` times the default builds in the v4 columns.

## Dedicated node (Ryzen 7 9800X3D)

`bench.yml` has, per input class, the default build and the v4 build side by side (Go
`GOAMD64=v4`, Rust `-C target-cpu=x86-64-v4`), and a table of the Rust bench with PGO and with
PGO + BOLT (cargo-pgo, trained on the same fixtures, so an upper bound; docs/performance.md).
Rust decodes with a reused `Decoder`, as Go does.

Before the faster decode loop and the parallel decode (amd64 Ryzen 7 9800X3D, Rust / Go time, Go
default GOMAXPROCS):

| class | default | v4 | PGO | PGO + BOLT |
|---|---|---|---|---|
| text | 0.94 | 0.97 | 0.91 | 0.91 |
| image | 0.98 | 1.03 | 0.92 | 0.92 |
| random | 0.93 | 0.93 | 0.95 | 0.96 |
| runs | 0.96 | 0.94 | 0.97 | 0.97 |
| zeros | 0.97 | 0.98 | 0.97 | 0.97 |
| png | 0.96 | 0.95 | 0.96 | 0.96 |
| tiff | 1.06 | 1.08 | 1.04 | 1.03 |
| all | 1.00 | 1.02 | 0.97 | 0.96 |

The worst file (`tiff-*-gradient`, long short-period matches) is 1.58. The work below fixed it.

After the faster decode loop and the parallel decode (Rust / Go time, Go default GOMAXPROCS):

| class | default | v4 | PGO | PGO + BOLT | worst file |
|---|---|---|---|---|---|
| text | 0.81 | 0.83 | 0.81 | 0.81 | 1.04 |
| image | 0.86 | 0.86 | 0.83 | 0.83 | 1.03 |
| random | 0.93 | 0.91 | 0.93 | 0.92 | 1.47 |
| runs | 0.91 | 0.89 | 0.92 | 0.92 | 0.98 |
| zeros | 0.87 | 0.90 | 0.87 | 0.86 | 0.93 |
| png | 0.96 | 0.98 | 0.95 | 0.95 | 1.03 |
| tiff | 1.03 | 1.05 | 1.02 | 1.02 | 1.04 |
| all | 0.92 | 0.93 | 0.91 | 0.91 | 1.47 |

The worst files are the 8 KiB `strategy-*-random` streams: 55 ns in Go, about 80 ns in Rust (a
fixed cost per call that came with the parallel decode build, open). The tiff class stays at 1.02 to
1.05: its noise images are literals and short matches, where Go's hand-written loop still runs
fewer instructions per symbol. One large stream (64 MiB of the fixtures' outputs, zlib level 6):
Go 47.6 ms, Rust `decode_parallel_into` on all CPUs 25.8 ms (0.54), Rust with `max_threads = 1`
47.4 ms (0.99).

## Dedicated node: the first unsafe fast loop


`.github/workflows/bench.yml` (not a required check; on `workflow_dispatch` and push to `main`)
runs `tests/parity/bench.sh`: Go and Rust decodes of the fixtures in one container, best of five runs
per file and side, as two tables in the job summary. Both decoders are serial (this crate spawns
no thread, and Go's decode has no goroutines), so the first table is Go at its default
`GOMAXPROCS` against Rust, and the second is the same Rust run against Go with `GOMAXPROCS=1`
(single-threaded). Each table has, per input class, the time-weighted Rust over Go ratio, the
geometric mean of the per-file ratios (`geo mean`) and the ratio of the worst file (`worst file`);
the script fails if the two sides decode different files or give a different output size.

On the dedicated amd64 node (Ryzen 7 9800X3D, BMI2 and AVX-512 VNNI), a run of the first
unsafe version (the fast-loop writes and the fused copy + Adler-32) gave the Rust time over the Go time, by
input class: text 0.97, image 0.99, random 0.77, runs 0.96, zeros 0.98, png 0.96, tiff 1.06, all
1.01. The worst file is `tiff-rgb512-gradient` at 1.59. The decode output is written straight
into the output `Vec`'s spare capacity, so there is no zero-fill pass.


## Go and Rust benchmark script


`bench.sh` and `.github/workflows/bench.yml`: `oracle -bench N` and `examples/bench.rs` decode each
fixture N times and print the ns per decode; `bench.sh` joins them into a ms/MB table. Go reuses
its `Decoder` and buffer, and so does Rust (`decode_into` on a cleared `Vec`). It prints two tables:
Go at its default `GOMAXPROCS`, and Go with `GOMAXPROCS=1` (the same Rust run). Both decoders are
serial; the second table takes the Go runtime's other cores away. It fails if the two sides do not
decode the same files or give a different output size for one.
