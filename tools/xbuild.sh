#!/usr/bin/env bash
# Cross-build from Linux: tools/xbuild.sh [release|dev] [extra cargo args]
# Needs: rustup target x86_64-pc-windows-gnu, cargo-zigbuild, zig on PATH.
#
# release -> target/x86_64-pc-windows-gnu/release/
# dev     -> target/dev/x86_64-pc-windows-gnu/dev-release/  (its own target
#            dir, so a dev DLL can never be mistaken for the release one;
#            profile dev-release = release with panic = "unwind")
set -euo pipefail
cd "$(dirname "$0")/.."
flavour="${1:-release}"; shift || true
case "$flavour" in
  release) extra=(--release) ;;
  dev) extra=(--profile dev-release --features dev --target-dir target/dev) ;;
  *) echo "usage: $0 [release|dev] [cargo args]" >&2; exit 2 ;;
esac
cargo zigbuild --target x86_64-pc-windows-gnu -p mhw-dlss -p mhw-dlss-launcher "${extra[@]}" "$@"
