#!/usr/bin/env bash
# Install system packages needed to compile FFmpeg + x264 from source.
# Only required once per machine. The compiled output is cached by Cargo in target/.
#
# After running this, `cargo build -p overlay` handles everything automatically.
# First build takes ~10 minutes; subsequent builds are fast (incremental).
#
# CI usage: run this in your setup step, then cache the `target/` directory
# keyed on Cargo.lock + the OS/arch. On cache hit the FFmpeg compile is skipped.

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "This script is for Linux only. See CLAUDE.md for Windows setup." >&2
    exit 1
fi

if ! command -v apt-get &>/dev/null; then
    echo "apt-get not found — install the following packages manually:" >&2
    echo "  libdbus-1-dev libopus-dev libasound2-dev git nasm libva-dev libx264-dev" >&2
    exit 1
fi

sudo apt-get update
sudo apt-get install -y \
    libdbus-1-dev \
    libopus-dev \
    libasound2-dev \
    git \
    nasm \
    libva-dev \
    libx264-dev \
    mingw-w64

rustup target add x86_64-pc-windows-gnu

echo ""
echo "Done."
echo "  Linux binary:   cargo build -p overlay"
echo "  Windows binary: ./scripts/cross-windows.sh --release"
echo ""
echo "Note: cargo cannot cross-compile for Windows on its own — build.rs requires FFMPEG_DIR,"
echo "which cross-windows.sh sets after building FFmpeg and x264 for the MinGW target."
echo "(First run compiles FFmpeg from source, ~10 min; cached on subsequent builds.)"
