#!/usr/bin/env bash
# Cross-compile overlay.exe for Windows from Linux using the MinGW toolchain.
#
# Prerequisite: ./scripts/setup-ffmpeg.sh (installs mingw-w64, nasm, etc.)
#
# This script:
#   1. Cross-compiles x264 and FFmpeg 7.1 for Windows (cached in build/ffmpeg-windows/).
#   2. Runs cargo with FFMPEG_DIR pointing at the pre-built result.
#
# Extra args are forwarded to cargo (e.g. --release):
#   ./scripts/cross-windows.sh --release
#
# Output: target/x86_64-pc-windows-gnu/{debug,release}/overlay.exe

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$REPO_ROOT/build/ffmpeg-windows"
INSTALL="$BUILD_DIR/install"
JOBS="$(nproc)"

CROSS=x86_64-w64-mingw32
CC="${CROSS}-gcc"
AR="${CROSS}-ar"
RANLIB="${CROSS}-ranlib"
STRIP="${CROSS}-strip"

echo "=== FFmpeg Windows cross-build (MinGW) ==="
echo "    Install prefix: $INSTALL"

# ── x264 ──────────────────────────────────────────────────────────────────────

if [ ! -f "$INSTALL/lib/libx264.a" ]; then
    mkdir -p "$BUILD_DIR"

    if [ ! -d "$BUILD_DIR/x264" ]; then
        echo "--- Cloning x264..."
        git clone --depth=1 https://code.videolan.org/videolan/x264.git "$BUILD_DIR/x264"
    fi

    echo "--- Building x264 for Windows..."
    # x264's configure does not support out-of-tree builds; run it from the source directory.
    # Also, it does not auto-derive CC from --host; set it explicitly.
    cd "$BUILD_DIR/x264"
    CC="$CC" AR="$AR" RANLIB="$RANLIB" STRIP="$STRIP" \
    ./configure \
        --host="$CROSS" \
        --prefix="$INSTALL" \
        --enable-static \
        --disable-cli \
        --disable-opencl
    make -j"$JOBS"
    make install
    cd "$REPO_ROOT"
fi

# ── FFmpeg 7.1 ────────────────────────────────────────────────────────────────

if [ ! -f "$INSTALL/lib/libavcodec.a" ]; then
    mkdir -p "$BUILD_DIR"

    if [ ! -d "$BUILD_DIR/ffmpeg" ]; then
        echo "--- Cloning FFmpeg 7.1..."
        git clone --depth=1 -b release/7.1 https://git.ffmpeg.org/ffmpeg.git "$BUILD_DIR/ffmpeg"
    fi

    echo "--- Building FFmpeg for Windows..."

    # FFmpeg cross-compile: it looks for ${cross_prefix}pkg-config (i.e.
    # x86_64-w64-mingw32-pkg-config) and falls back to `false` if not found,
    # silently disabling all pkg-config library checks.  Create a thin wrapper
    # that calls the system pkg-config restricted to our install prefix.
    mkdir -p "$BUILD_DIR/bin"
    cat > "$BUILD_DIR/bin/x86_64-w64-mingw32-pkg-config" <<EOF
#!/bin/sh
exec env PKG_CONFIG_LIBDIR="$INSTALL/lib/pkgconfig" pkg-config "\$@"
EOF
    chmod +x "$BUILD_DIR/bin/x86_64-w64-mingw32-pkg-config"

    mkdir -p "$BUILD_DIR/ffmpeg-build"
    cd "$BUILD_DIR/ffmpeg-build"
    PATH="$BUILD_DIR/bin:$PATH" \
    "$BUILD_DIR/ffmpeg/configure" \
        --target-os=mingw32 \
        --arch=x86_64 \
        --cross-prefix="${CROSS}-" \
        --prefix="$INSTALL" \
        --enable-gpl \
        --enable-libx264 \
        --enable-static \
        --disable-shared \
        --disable-programs \
        --disable-doc \
        --disable-network \
        --disable-everything \
        --enable-encoder=libx264 \
        --enable-decoder=h264 \
        --enable-parser=h264 \
        --enable-swscale \
        --extra-cflags="-I$INSTALL/include" \
        --extra-ldflags="-L$INSTALL/lib"
    make -j"$JOBS"
    make install

    cd "$REPO_ROOT"
fi

echo ""
echo "=== Cargo cross-compile ==="
FFMPEG_DIR="$INSTALL" \
    cargo build -p overlay --target x86_64-pc-windows-gnu "$@"

echo ""
PROFILE="debug"
for arg in "$@"; do [ "$arg" = "--release" ] && PROFILE="release"; done
echo "Output: target/x86_64-pc-windows-gnu/$PROFILE/overlay.exe"
