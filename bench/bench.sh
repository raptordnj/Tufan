#!/usr/bin/env bash
# Reproducible small-file / large-file benchmark: tufan vs cp.
# Usage: bench/bench.sh [WORKDIR] [SMALL_FILES] [LARGE_MIB]
# Run from a release build (cargo build --release). Results depend on
# hardware, filesystem and cache state; record them with the output.
set -euo pipefail
W=${1:-/tmp/tufan-bench}
N=${2:-100000}
L=${3:-1024}
BIN=${TUFAN:-$(dirname "$0")/../target/release/tufan}
rm -rf "$W"; mkdir -p "$W/small" "$W/out"
echo "# host: $(uname -srm)  fs: $(stat -f -c %T "$W")  cpus: $(nproc)"
echo "# generating $N small files and a ${L} MiB file in $W"
python3 - "$W/small" "$N" <<'PY'
import os, sys
root, n = sys.argv[1], int(sys.argv[2])
for i in range(n):
    d = os.path.join(root, f"d{i // 1000}")
    if i % 1000 == 0: os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, f"f{i}.js"), "wb") as f: f.write(os.urandom(64 + i % 4096))
PY
head -c $((L << 20)) /dev/urandom > "$W/large.bin"
sync

run() { local label=$1; shift; rm -rf "$W/out"; mkdir -p "$W/out"; sync
  local t0=$(date +%s.%N); "$@" >/dev/null; local t1=$(date +%s.%N)
  printf '%-34s %8.2f s\n' "$label" "$(awk "BEGIN{print $t1 - $t0}")"; }

J="$W/journal.sqlite3"
run "cp -r small"                cp -r "$W/small" "$W/out/"
run "tufan small --verify fast"     "$BIN" -q --journal "$J" copy --verify fast "$W/small" "$W/out"
run "tufan small --verify verified" "$BIN" -q --journal "$J" copy "$W/small" "$W/out"
run "tufan small --verify durable"  "$BIN" -q --journal "$J" copy --verify durable "$W/small" "$W/out"
run "cp large"                   cp "$W/large.bin" "$W/out/"
run "tufan large --verify fast"     "$BIN" -q --journal "$J" copy --verify fast "$W/large.bin" "$W/out"
run "tufan large --verify verified" "$BIN" -q --journal "$J" copy "$W/large.bin" "$W/out"
echo "# note: page cache is warm after generation; drop caches (as root) for cold runs"
