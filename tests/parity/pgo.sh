#!/bin/sh
# The bench example with PGO, and with PGO + BOLT (cargo-pgo), trained on the fixtures:
# /usr/local/bin/bench-pgo and /usr/local/bin/bench-pgo-bolt (docs/performance.md).
# Best effort: a step that fails says so and leaves its binary out, and bench.sh then has no row
# for it. Run from the repo root in the bench stage (.github/ci/Dockerfile).
set -u
d=tests/data/fixtures
host=$(rustc -vV | sed -n 's/^host: //p')
out=target/$host/release/examples
# target/ is a cache mount: old profiles would merge into the new ones.
rm -rf target/pgo-profiles target/bolt-profiles
if cargo pgo build -- --locked --example bench \
  && "$out/bench" $d 30 >/dev/null \
  && cargo pgo optimize build -- --locked --example bench \
  && cp "$out/bench" /usr/local/bin/bench-pgo; then
  echo "pgo: bench-pgo built"
else
  echo "pgo: the PGO build failed"
  exit 0
fi
if ! command -v llvm-bolt >/dev/null || ! command -v merge-fdata >/dev/null; then
  echo "pgo: no llvm-bolt or merge-fdata, so no BOLT row"
  exit 0
fi
inst=$(cargo pgo bolt build --with-pgo -- --locked --example bench 2>&1 | tee /dev/stderr \
  | sed -n 's/.*\(target\/[^ ]*-bolt-instrumented\).*/\1/p' | tail -n 1)
[ -n "$inst" ] || inst=$(find target -name 'bench-bolt-instrumented' -newer Cargo.toml | head -n 1)
if [ -n "$inst" ] && "$inst" $d 30 >/dev/null \
  && cargo pgo bolt optimize --with-pgo -- --locked --example bench; then
  opt=$(find target -name 'bench-bolt-optimized' -newer "$inst" | head -n 1)
  if [ -n "$opt" ]; then
    cp "$opt" /usr/local/bin/bench-pgo-bolt && echo "pgo: bench-pgo-bolt built"
    exit 0
  fi
fi
echo "pgo: the BOLT build failed, so no BOLT row"
