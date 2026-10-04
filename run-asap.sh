#!/usr/bin/env bash
# run-asap.sh — build & run dengan ASAP driver, selalu paksa recompile.
#
# Kenapa perlu? Mengganti RUSTC_WRAPPER saja tidak selalu membuat cargo
# recompile (biner lama yang jalan → hasilnya terlihat seperti baseline).
# Script ini memaksa recompile target dulu agar driver benar-benar dijalankan.
#
#   ./run-asap.sh                  # project di current dir, mode debug
#   ./run-asap.sh --release        # project di current dir, mode release
#   ./run-asap.sh -p tes           # crate tes di workspace, mode debug
#   ./run-asap.sh -p tes --release # crate tes di workspace, mode release
#
set -e
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CALLER_DIR="$(pwd)"

# Ambil nilai -p/--package bila ada.
PKG=""
WANT_PKG=0
for a in "$@"; do
    if [ "$WANT_PKG" = "1" ]; then
        PKG="$a"
        WANT_PKG=0
    elif [ "$a" = "-p" ] || [ "$a" = "--package" ]; then
        WANT_PKG=1
    fi
done

echo "=== Build driver ==="
cd "$SCRIPT_DIR"
cargo +nightly build -p asap-driver --release
DRIVER="$SCRIPT_DIR/target/release/asap-driver"
echo "Driver: $DRIVER"

if [ -n "$PKG" ]; then
    # Paket workspace eksplisit: bersihkan artefaknya agar recompile.
    echo ""
    echo "=== Force recompile package: $PKG ==="
    cargo clean -p "$PKG" 2>/dev/null || true
    RUN_DIR="$SCRIPT_DIR"
else
    # Tanpa -p: pakai project Rust di current dir pemanggil.
    RUN_DIR="$CALLER_DIR"
    echo ""
    echo "=== Target: project di current dir ($RUN_DIR) ==="
    if [ -f "$RUN_DIR/src/main.rs" ]; then
        touch "$RUN_DIR/src/main.rs"
    fi
    if [ -f "$RUN_DIR/src/lib.rs" ]; then
        touch "$RUN_DIR/src/lib.rs"
    fi
fi

echo ""
echo "=== Run dengan ASAP driver: cargo +nightly run $* ==="
cd "$RUN_DIR"
RUSTC_WRAPPER="$DRIVER" cargo +nightly run "$@"
