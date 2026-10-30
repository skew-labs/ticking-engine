#!/usr/bin/env bash
# Builds programs/mee for Solana (SBF) with Anza's platform-tools and,
# optionally, runs the program tests against the compiled binary.
#
#   scripts/build-sbf.sh            # build target/sbf/sbpf-solana-solana/release/mee.so
#   scripts/build-sbf.sh test       # build, then run the program tests on it
#   scripts/build-sbf.sh cu         # build, then print compute units per instruction
#
# Same result as `cargo build-sbf`, without needing the Solana CLI: the
# platform-tools release is downloaded once into $PLATFORM_TOOLS_DIR.
set -euo pipefail

VERSION="${PLATFORM_TOOLS_VERSION:-v1.58}"
TOOLS="${PLATFORM_TOOLS_DIR:-$HOME/.cache/mee/platform-tools-$VERSION}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/target/sbf"

if [ ! -x "$TOOLS/rust/bin/rustc" ]; then
  mkdir -p "$TOOLS"
  url="https://github.com/anza-xyz/platform-tools/releases/download/$VERSION/platform-tools-linux-x86_64.tar.bz2"
  echo "downloading $url"
  curl -fsSL "$url" | tar -xj -C "$TOOLS"
fi

cd "$ROOT"
RUSTC="$TOOLS/rust/bin/rustc" CARGO_TARGET_DIR="$OUT" \
  "$TOOLS/rust/bin/cargo" build --release --target sbpf-solana-solana -p mee --lib
SO="$OUT/sbpf-solana-solana/release/mee.so"
echo "built $SO ($(stat -c %s "$SO") bytes)"

case "${1:-}" in
  test)
    SBF_OUT_DIR="$(dirname "$SO")" cargo test -p mee
    ;;
  cu)
    SBF_OUT_DIR="$(dirname "$SO")" MEE_CU_REPORT=1 cargo test -q -p mee -- --test-threads=1 --nocapture 2>&1 |
      grep -E '^(MEE_CU|MEE_CU_SUMMARY)' |
      awk -F'\t' '/^MEE_CU\t/ { n[$2]++; if ($3 > max[$2]) max[$2] = $3; sum[$2] += $3; next } { print }
        END { for (k in n) printf "%-42s max %7d  avg %7d  (%d runs)\n", k, max[k], sum[k] / n[k], n[k] }' |
      sort -k3 -n -r
    ;;
esac
