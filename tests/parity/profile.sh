#!/bin/sh
# tests/parity/profile.sh <filter>...: the fixtures whose name contains each filter, decoded by the
# Rust bench example and by the Go oracle (GOMAXPROCS=1) under callgrind: the instruction counts of
# both, and the Rust report (instructions per function and line, inclusive and exclusive).
set -eu
for f in "$@"; do
  echo "=== $f"
  rm -rf /tmp/p && mkdir /tmp/p && cp tests/data/fixtures/*"$f"* /tmp/p/
  ls /tmp/p
  GOMAXPROCS=1 valgrind --tool=callgrind --callgrind-out-file=/tmp/go.out oracle -bench 20 /tmp/p >/dev/null 2>/tmp/go.err || { cat /tmp/go.err; exit 1; }
  echo "Go:"; grep -E "Collected" /tmp/go.err || true
  callgrind_annotate --threshold=90 /tmp/go.out 2>&1 | head -40
  valgrind --tool=callgrind --callgrind-out-file=/tmp/cg.out bench /tmp/p 20 >/dev/null 2>/tmp/cg.err || { cat /tmp/cg.err; exit 1; }
  echo "Rust:"; grep -E "Collected" /tmp/cg.err || true
  callgrind_annotate --auto=yes --threshold=96 --context=1 /tmp/cg.out 2>&1 | head -500
done
# The disassembly of the decode loops (Rust AVX2 pair loop, Go BMI2 pair loop), to compare them.
if [ -n "${PROFILE_ASM:-}" ]; then
  echo "=== asm: Rust run_avx2::<true>"
  objdump -d --no-show-raw-insn -C "$(command -v bench)" | awk '/^[0-9a-f]+ <.*run_avx2.*true.*>:$/{p=1} p&&/^$/{exit} p' | sed 's/^ *[0-9a-f]*:\t//' | head -900
  echo "=== asm: Go fastLoopBMI2Pair"
  objdump -d --no-show-raw-insn "$(command -v oracle)" | awk '/<.*fastLoopBMI2Pair.*>:$/{p=1} p&&/^$/{exit} p' | sed 's/^ *[0-9a-f]*:\t//' | head -400
fi
