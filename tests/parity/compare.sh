#!/bin/sh
# compare.sh <go-manifest> <rust-manifest> <min-lines>: the parity check of .github/ci/Dockerfile. Fails on
# fewer than <min-lines> lines in the Go manifest, a failed sort, a different number of lines, or on any difference (it prints the first 50 differing lines, `<` Go and `>` Rust, and
# their number). The comparison ignores line order: both sides are sorted bytewise first, as the
# two oracles emit the operations of one input in different orders. A key can occur twice (a
# DecodeLimit n that is both a fixed limit and a window limit); the sorted multisets still match.
n=$(wc -l <"$1")
echo "$n manifest lines"
[ "$n" -ge "$3" ] || { echo "fewer than $3 manifest lines"; exit 1; }
t=$(mktemp -d) || exit 1
trap 'rm -rf "$t"' EXIT
# A failed sort (a full disk) must not pass as two equal files.
LC_ALL=C sort "$1" >"$t/go" || { echo "sort of $1 failed"; exit 1; }
LC_ALL=C sort "$2" >"$t/rs" || { echo "sort of $2 failed"; exit 1; }
r=$(wc -l <"$t/rs")
[ "$n" -eq "$r" ] && cmp -s "$t/go" "$t/rs" && exit 0
[ "$n" -eq "$r" ] || echo "line counts differ: Go $n, Rust $r"
diff "$t/go" "$t/rs" | grep '^[<>]' >"$t/diff"
head -50 "$t/diff"
echo "$(wc -l <"$t/diff") differing lines"
exit 1
