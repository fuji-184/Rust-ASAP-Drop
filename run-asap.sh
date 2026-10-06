#!/usr/bin/env bash
# run-asap.sh — jalankan command cargo APAPUN dengan ASAP driver.
#
# Command-nya passthrough penuh ke cargo (bukan fixed `run`), jadi semua
# command cargo didukung: run, build, check, test, expand, ...
#
#   ./run-asap.sh run -p tes             # ala `cargo run -p tes`
#   ./run-asap.sh run --release          # project di current dir
#   ./run-asap.sh build -p tes
#   ./run-asap.sh check
#   ./run-asap.sh test -- --nocapture
#   ./run-asap.sh expand -p tes          # butuh cargo-expand
#   ./run-asap.sh                        # tanpa arg = `run`
#
# Kenapa perlu script ini? Mengganti RUSTC_WRAPPER saja tidak selalu membuat
# cargo recompile (biner lama yang jalan → hasilnya terlihat seperti
# baseline). Script ini memaksa recompile target dulu agar driver
# benar-benar dijalankan.
#
set -e
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CALLER_DIR="$(pwd)"

# Tanpa arg sama sekali = `run` (tetap dukung kebiasaan lama).
if [ "$#" = "0" ]; then
    set -- run
fi

# Ambil nilai -p/--package bila ada (untuk force-recompile paket itu).
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
echo "=== cargo +nightly $* (dengan ASAP driver) ==="
cd "$RUN_DIR"
RUSTC_WRAPPER="$DRIVER" cargo +nightly "$@"
