#!/bin/sh
# Decode time of the fixtures, Go and Rust on the same inputs, as markdown tables:
# tests/parity/bench.sh [N]. Needs on PATH: `oracle` and `bench` (default builds), `oracle-v4` and
# `bench-v4` (Go GOAMD64=v4, Rust -C target-cpu=x86-64-v4), and, if tests/parity/pgo.sh made
# them, `bench-pgo` and `bench-pgo-bolt`. Run from the repo root.
set -eu
export LC_ALL=C
n=${1:-300}
grep -m1 'model name' /proc/cpuinfo >&2 || true
grep -m1 '^flags' /proc/cpuinfo | tr ' ' '\n' | grep -E '^(avx2|bmi2|avx512(f|bw|vl|_vnni))$' | tr '\n' ' ' >&2 || true
echo >&2
d=tests/data/fixtures
t=/tmp/bench
mkdir -p $t
# A v4 build dies with SIGILL on a CPU without AVX-512 (some hosted runners): then the v4 columns
# time the default builds.
if ! grep -qw avx512f /proc/cpuinfo; then
  echo "bench: no AVX-512, the v4 columns repeat the default builds" >&2
  mkdir -p $t/bin
  ln -sf "$(command -v oracle)" $t/bin/oracle-v4
  ln -sf "$(command -v bench)" $t/bin/bench-v4
  PATH=$t/bin:$PATH
fi
# The Rust variants that exist.
rs="bench bench-v4"
for b in bench-pgo bench-pgo-bolt; do
  if command -v $b >/dev/null; then rs="$rs $b"; fi
done
# Best of five runs per file and side: the machine is shared, and one run can be slow.
best() {
  awk -F'\t' '!($1 in m) || $3 < m[$1] { m[$1] = $3; b[$1] = $2 } END { for (k in m) printf "%s\t%s\t%s\n", k, b[k], m[k] }' "$@" | sort
}
# Go runs at its default GOMAXPROCS and at 1. Both decoders are serial, but the Go runtime (GC,
# scheduler) can use other cores; each Rust run is the same in both tables.
for i in 1 2 3 4 5; do
  for g in oracle oracle-v4; do
    $g -bench "$n" $d >$t/$g.$i.tsv
    GOMAXPROCS=1 $g -bench "$n" $d >$t/$g-1p.$i.tsv
  done
  for r in $rs; do
    $r $d "$n" >$t/$r.$i.tsv
  done
done
for s in oracle oracle-1p oracle-v4 oracle-v4-1p $rs; do
  best $t/$s.?.tsv >$t/$s.tsv
done

# join2 <go> <rust> <out>: name, Go out bytes, Go ns, Rust out bytes, Rust ns. A file only one side
# decodes, or decoded to different sizes, fails the run.
join2() {
  join -t "$(printf '\t')" "$t/$1.tsv" "$t/$2.tsv" >"$3"
  [ "$(wc -l <"$t/$1.tsv")" -eq "$(wc -l <"$3")" ] && [ "$(wc -l <"$t/$2.tsv")" -eq "$(wc -l <"$3")" ] \
    || { echo "bench: the $1 and $2 file sets differ" >&2; exit 1; }
  awk -F'\t' '$2 != $4 { print "bench: output size differs: " $1 " Go " $2 " Rust " $4 > "/dev/stderr"; bad = 1 } END { exit bad }' "$3" || exit 1
}

# class <joined>: per input class, "class files MB go_ns rust_ns geo worst" (ns summed). ns per
# byte is ms per MB. The time-weighted ratio is sum(Rust ns) / sum(Go ns); geo is the geometric mean
# of the per-file ratios and worst the worst file's, so a few heavy files cannot carry a class.
class() {
  awk -F'\t' '
function add(k) {
  b[k] += $2; g[k] += $3; r[k] += $5; c[k]++
  if ($3 > 0 && $5 > 0) { q = $5 / $3; lg[k] += log(q); nl[k]++; if (q > w[k]) w[k] = q }
}
{
  k = "other"
  if (match($1, /text|image|random|runs|zeros|png|tiff/)) k = substr($1, RSTART, RLENGTH)
  add(k); add("all")
}
END {
  m = split("text image random runs zeros png tiff other all", ks, " ")
  for (i = 1; i <= m; i++) {
    k = ks[i]
    if (b[k]) printf "%s %d %.2f %.0f %.0f %.2f %.2f\n", k, c[k], b[k] / 1e6, g[k], r[k], nl[k] ? exp(lg[k] / nl[k]) : 0, w[k]
  }
}' "$1"
}

# table <go default> <go v4>: the default build and the v4 build, side by side.
table() {
  join2 "$1" bench $t/a.tsv
  join2 "$2" bench-v4 $t/b.tsv
  class $t/a.tsv >$t/a.cls
  class $t/b.tsv >$t/b.cls
  echo "| input | files | MB out | Go ms/MB | Rust ms/MB | Rust / Go | geo mean | worst file | Go v4 ms/MB | Rust v4 ms/MB | v4 Rust / Go | v4 geo mean | v4 worst file |"
  echo "|---|---|---|---|---|---|---|---|---|---|---|---|---|"
  # Both list the same classes in the same order (same files).
  paste -d ' ' $t/a.cls $t/b.cls | awk '{ mb = $3 * 1e6
    printf "| %s | %d | %.2f | %.3f | %.3f | %.2f | %.2f | %.2f | %.3f | %.3f | %.2f | %.2f | %.2f |\n",
      $1, $2, $3, $4 / mb, $5 / mb, $5 / $4, $6, $7, $11 / mb, $12 / mb, $12 / $11, $13, $14 }'
}

echo "#### default (Go GOMAXPROCS = all cores)"
echo
table oracle oracle-v4
echo
echo "#### single-threaded (GOMAXPROCS=1)"
echo
table oracle-1p oracle-v4-1p

# PGO and PGO + BOLT against the default Rust build and Go (default GOMAXPROCS).
if [ "$rs" != "bench bench-v4" ]; then
  echo
  echo "#### Rust with PGO and BOLT (cargo-pgo, trained on these fixtures), against Go default"
  echo
  hdr="| input | Rust / Go"
  sep="|---|---"
  for r in $rs; do
    case $r in bench | bench-v4) continue ;; esac
    join2 oracle $r $t/$r.j
    class $t/$r.j >$t/$r.cls
    hdr="$hdr | $r / Go | $r worst file"
    sep="$sep|---|---"
  done
  echo "$hdr |"
  echo "$sep|"
  join2 oracle bench $t/a.tsv
  class $t/a.tsv | awk '{ printf "%s %.2f\n", $1, $5 / $4 }' >$t/p.cls
  for r in $rs; do
    case $r in bench | bench-v4) continue ;; esac
    awk '{ printf "%.2f %.2f\n", $5 / $4, $7 }' $t/$r.cls | paste -d ' ' $t/p.cls - >$t/p2.cls
    mv $t/p2.cls $t/p.cls
  done
  awk '{ printf "|"; for (i = 1; i <= NF; i++) printf " %s |", $i; printf "\n" }' $t/p.cls
fi

# One large stream (64 MiB of the fixtures' outputs, zlib level 6: `oracle -qlarge`, made in the
# bench image at /large): Go (serial, it has no parallel decode) against Rust's parallel decode
# with all CPUs and with max_threads 1. Best of five.
if [ -d /large ]; then
  for i in 1 2 3 4 5; do
    oracle -bench 3 /large >$t/lg-go.$i.tsv
    bench /large 3 >$t/lg-rs.$i.tsv
    bench /large 3 1 >$t/lg-rs1.$i.tsv
  done
  for s in lg-go lg-rs lg-rs1; do best $t/$s.?.tsv >$t/$s.tsv; done
  join -t "$(printf '\t')" $t/lg-go.tsv $t/lg-rs.tsv | join -t "$(printf '\t')" - $t/lg-rs1.tsv >$t/lg.tsv
  echo
  echo "#### one large stream: Rust parallel decode (all CPUs, and max_threads 1) against Go"
  echo
  echo "| stream | MB out | Go ms | Rust ms | Rust / Go | Rust 1 thread ms | 1 thread Rust / Go |"
  echo "|---|---|---|---|---|---|---|"
  awk -F'\t' '$2 != $4 || $2 != $6 { print "bench: output size differs: " $1 > "/dev/stderr"; exit 1 }
    { printf "| %s | %.1f | %.2f | %.2f | %.2f | %.2f | %.2f |\n", $1, $2 / 1e6, $3 / 1e6, $5 / 1e6, $5 / $3, $7 / 1e6, $7 / $3 }' $t/lg.tsv
fi

# Per file, all of them: the Go and Rust ns and the ratio, the worst first (stderr, for reading
# the log).
join2 oracle bench $t/a.tsv
awk -F'\t' '$3 > 0 { printf "%s\t%d\t%d\t%d\t%.2f\n", $1, $2, $3, $5, $5 / $3 }' $t/a.tsv | sort -t"$(printf '\t')" -k5 -nr >&2
