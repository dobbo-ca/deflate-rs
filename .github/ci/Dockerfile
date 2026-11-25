# CI image: every check runs in a stage of this file, built by BuildKit on the GitHub-hosted runner.
# The crate has no dependencies, so there is no `cargo fetch` layer.
# Every tool is installed in a stage before `COPY . .`, so a new commit does not install it again.
FROM rust:1.99.1-bookworm AS toolchain
RUN rustup component add rustfmt clippy && rustup target add aarch64-unknown-linux-gnu
WORKDIR /src

FROM toolchain AS src
COPY . .

# The checks run in the build, so a red check fails the build; a rerun of the same source is a
# BuildKit cache hit. The registry and target dirs are cache mounts, so they persist across builds.
FROM src AS ci
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    cargo fmt --check
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    cargo clippy --locked --all-targets -- -D warnings
# The NEON module is not compiled for x86_64, so lint the aarch64 target too.
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    cargo clippy --locked --all-targets --target aarch64-unknown-linux-gnu -- -D warnings
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    cargo test --locked
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    DEFLATE_SIMD=scalar cargo test --locked
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    cargo build --locked --release --target aarch64-unknown-linux-gnu

# The arm64 job builds this stage on a native arm64 runner: the whole suite, both SIMD tiers (NEON,
# then DEFLATE_SIMD=scalar). Go parity on aarch64 is the parity-arm64 stage.
FROM src AS arm64
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    cargo test --locked
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    DEFLATE_SIMD=scalar cargo test --locked

# Go parity: the Go oracle writes the inputs and decodes them into a manifest (one line per input and
# operation, with the length and FNV-1a 64 of the output: docs/parity.md); the Rust example decodes
# the same inputs into its own manifest; the two manifests must be equal.
FROM golang:1.27-bookworm AS go-oracle
ENV GOTOOLCHAIN=local CGO_ENABLED=0
WORKDIR /src
COPY tests/parity/go tests/parity/go
RUN --mount=type=cache,id=deflate-go-build,target=/root/.cache/go-build cd tests/parity/go/oracle && go build -o /out/oracle .

# The hand-built edge streams of tests/edge.rs, so Go decodes them too.
FROM src AS edge
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    DEFLATE_EDGE_DUMP=/edge cargo test --locked --release --test edge dump_edge_streams

# The inputs are a tree of some 100,000 files, so they are made, decoded and tarred in one RUN and
# the tree is deleted at its end: the layer holds only /go-res (the inputs as two tars, and the two
# manifests of about 50 MB), and the Rust stages untar into a temporary directory.
FROM go-oracle AS go-run
COPY tests/data/fixtures tests/data/fixtures
COPY tests/fuzz/seeds tests/fuzz/seeds
COPY --from=edge /edge /edge
# Inputs: the generated ones, the edge streams, and the deflate-go fuzz seeds as raw and zlib.
# The subset (/sub, /go-res/sub.tar) is what the aarch64 oracle and the scalar tier run under qemu
# (stage parity-arm64): every input that is not a mutation (`~` in its name), and every 20th
# mutation. Go decodes it again, so the two manifests have the same names.
RUN /out/oracle -gen tests/data/fixtures /inputs \
    && cp /edge/* /inputs/ \
    && for f in tests/fuzz/seeds/*; do b=$(basename $f); cp $f /inputs/seed-$b.raw; cp $f /inputs/seed-$b.zlib; done \
    && ls /inputs | wc -l && du -sh /inputs \
    && mkdir /go-res \
    && /out/oracle /inputs /go-res/manifest.txt \
    && tar -cf /go-res/inputs.tar -C /inputs . \
    && mkdir /sub && cp -r /inputs/dicts /sub/ \
    && ls /inputs | grep -v '^dicts$' | awk '!/~/ || NR % 20 == 0' | (cd /inputs && xargs cp -t /sub) \
    && /out/oracle /sub /go-res/sub-manifest.txt \
    && tar -cf /go-res/sub.tar -C /sub . \
    && ls /sub | wc -l && wc -l /go-res/*.txt && du -sh /go-res \
    && rm -rf /inputs /sub

# /go-res is a bind mount from go-run, not COPY: a COPY after `COPY . .` would run on every commit.
# The mount is `rw` (the writes are thrown away after the RUN): BuildKit keys a read-only mount by a
# hash of its content, but a rw mount by go-run's own cache key. The inputs are untarred into /tmp
# and deleted in the same RUN, so the layer (and the cache export) stays empty.
FROM src AS parity
# The scalar tier (no BMI2 loop, scalar Adler-32) runs the subset of parity-arm64.
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    --mount=type=bind,from=go-run,source=/go-res,target=/go-res,rw \
    mkdir /tmp/in /tmp/sub \
    && tar -xf /go-res/inputs.tar -C /tmp/in && tar -xf /go-res/sub.tar -C /tmp/sub \
    && cargo run --locked --release --example oracle -- /tmp/in /tmp/rust.txt \
    && echo "$(ls /tmp/in | wc -l) inputs" \
    && sh tests/parity/compare.sh /go-res/manifest.txt /tmp/rust.txt 100000 \
    && DEFLATE_SIMD=scalar cargo run --locked --release --example oracle -- /tmp/sub /tmp/rust-scalar.txt \
    && sh tests/parity/compare.sh /go-res/sub-manifest.txt /tmp/rust-scalar.txt 10000 \
    && rm -rf /tmp/in /tmp/sub /tmp/rust.txt /tmp/rust-scalar.txt

# aarch64 against Go: the parity-arm64 job builds this stage on a native arm64 runner (NEON adler32
# and the aarch64 build of the decode loop), on the subset, once per SIMD tier; the manifest must
# equal Go's.
FROM src AS parity-arm64
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    --mount=type=bind,from=go-run,source=/go-res,target=/go-res,rw \
    mkdir /tmp/sub && tar -xf /go-res/sub.tar -C /tmp/sub \
    && cargo run --locked --release --example oracle -- /tmp/sub /tmp/arm.txt \
    && echo "$(ls /tmp/sub | wc -l) inputs" \
    && sh tests/parity/compare.sh /go-res/sub-manifest.txt /tmp/arm.txt 10000 \
    && DEFLATE_SIMD=scalar cargo run --locked --release --example oracle -- /tmp/sub /tmp/arm-scalar.txt \
    && sh tests/parity/compare.sh /go-res/sub-manifest.txt /tmp/arm-scalar.txt 10000 \
    && rm -rf /tmp/sub /tmp/arm.txt /tmp/arm-scalar.txt

# Fuzzing (fuzz.yml): nightly and cargo-fuzz, with the targets built into the image. The
# workflow runs it with `docker run`, so a BuildKit cache hit cannot skip the fuzzing.
FROM toolchain AS fuzz-tools
RUN rustup toolchain install nightly --profile minimal \
    && cargo install cargo-fuzz --locked --version 0.13.1

FROM fuzz-tools AS fuzz
COPY . .
RUN cargo +nightly fuzz build --fuzz-dir tests/fuzz

# Miri (ci.yml `miri`): the non-SIMD unsafe kernels, the decode loop that writes through a raw
# pointer into spare capacity and the copy into spare capacity with `set_len`, run under Miri
# through their small unit tests (the `miri_` names in src/fast.rs and src/adler32/mod.rs). Under
# Miri the dispatch picks the generic loop and the scalar Adler-32.
# No `cargo miri setup` here: a sysroot built in this layer, then reused by `miri test` in /src,
# failed with "detected a concurrent sysroot build with different settings"; `miri test` builds
# its own sysroot (a minute or two).
FROM fuzz-tools AS miri-tools
RUN rustup +nightly component add miri rust-src

FROM miri-tools AS miri
COPY . .
RUN --mount=type=cache,id=deflate-miri-target,target=/src/target cargo +nightly miri test --locked --lib miri_

# The Go oracle built with GOAMD64=v4, for the bench's v4 column (docs/benchmarks.md). Its own
# stage, so go-oracle and the parity inputs of go-run do not change with it.
FROM golang:1.27-bookworm AS go-oracle-v4
ENV GOTOOLCHAIN=local CGO_ENABLED=0 GOAMD64=v4
WORKDIR /src
COPY tests/parity/go tests/parity/go
RUN --mount=type=cache,id=deflate-go-build,target=/root/.cache/go-build cd tests/parity/go/oracle && go build -o /out/oracle .

# cargo-pgo, the LLVM tools that match rustc (llvm-profdata) and BOLT (Debian's bolt-16: llvm-bolt
# and merge-fdata, and its runtime libbolt_rt_*.a, which llvm-bolt looks for in /usr/local/lib),
# for the bench's PGO and PGO + BOLT rows. BOLT is best effort: tests/parity/pgo.sh
# says so in the log when it is missing, and the bench then has no BOLT row.
FROM toolchain AS pgo-tools
RUN rustup component add llvm-tools-preview \
    && cargo install cargo-pgo --locked --version 0.2.9 \
    && apt-get update && (apt-get install -y --no-install-recommends bolt-16 || true) && rm -rf /var/lib/apt/lists/* \
    && for t in llvm-bolt merge-fdata; do p=$(ls /usr/lib/llvm-*/bin/$t 2>/dev/null | tail -n 1); if [ -n "$p" ]; then ln -sf "$p" /usr/local/bin/$t; fi; done \
    && for l in /usr/lib/llvm-*/lib/libbolt_rt_*.a; do if [ -e "$l" ]; then ln -sf "$l" /usr/local/lib/; fi; done

# Benchmark (bench.yml): the Go oracle and the Rust bench example in one image, each as the default
# build and as an x86-64-v4 build (Go GOAMD64=v4, Rust -C target-cpu=x86-64-v4), plus the Rust
# bench with PGO and with PGO + BOLT (cargo-pgo, trained on the fixtures: tests/parity/pgo.sh).
FROM pgo-tools AS bench
COPY . .
COPY --from=go-oracle /out/oracle /usr/local/bin/oracle
COPY --from=go-oracle-v4 /out/oracle /usr/local/bin/oracle-v4
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    cargo build --locked --release --example bench && cp target/release/examples/bench /usr/local/bin/bench \
    && RUSTFLAGS="-C target-cpu=x86-64-v4" cargo build --locked --release --example bench --target-dir target/v4 \
    && cp target/v4/release/examples/bench /usr/local/bin/bench-v4 \
    && sh tests/parity/pgo.sh \
    && mkdir /large && oracle -qlarge tests/data/fixtures /large/large-mixed.zlib

# Profile (profile.yml): the bench example with line tables and the Go oracle, under valgrind's
# callgrind, on the fixtures that tests/parity/profile.txt names (tests/parity/profile.sh).
FROM toolchain AS profile-tools
RUN apt-get update && apt-get install -y --no-install-recommends valgrind && rm -rf /var/lib/apt/lists/*

FROM profile-tools AS profile
COPY . .
COPY --from=go-oracle /out/oracle /usr/local/bin/oracle
RUN --mount=type=cache,id=deflate-registry,target=/usr/local/cargo/registry --mount=type=cache,id=deflate-target,target=/src/target \
    CARGO_PROFILE_RELEASE_DEBUG=line-tables-only cargo build --locked --release --example bench --target-dir target/profile \
    && cp target/profile/release/examples/bench /usr/local/bin/bench
