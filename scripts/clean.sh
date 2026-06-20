#!/usr/bin/env bash
# Remove build artifacts.
#
# Usage:
#   ./scripts/clean.sh           # remove build/ffmpeg-windows/ (native dep cache)
#   ./scripts/clean.sh --cargo   # also run `cargo clean` (removes target/)
#   ./scripts/clean.sh --all     # same as --cargo

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CARGO=0
for arg in "$@"; do
    case "$arg" in
        --cargo|--all) CARGO=1 ;;
        *) echo "Unknown argument: $arg" >&2; exit 1 ;;
    esac
done

echo "--- Removing build/ffmpeg-windows/..."
rm -rf "$REPO_ROOT/build/ffmpeg-windows"

if [ "$CARGO" -eq 1 ]; then
    echo "--- Running cargo clean..."
    cargo clean --manifest-path "$REPO_ROOT/Cargo.toml"
fi

echo "Done."
