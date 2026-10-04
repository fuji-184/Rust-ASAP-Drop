#!/usr/bin/env bash
# build.sh — Build semua crate dan jalankan test

set -e
cd "$(dirname "$0")"

echo "=== 1. Setup nightly ==="
rustup toolchain install nightly --component rustc-dev llvm-tools-preview 2>/dev/null || true

echo ""
echo "=== 2. Build proc macro (stable ok) ==="
cargo +nightly build -p asap-macro --release

echo ""
echo "=== 3. Build runtime ==="
cargo +nightly build -p asap-runtime --release

echo ""
echo "=== 4. Build driver (butuh rustc-dev) ==="
cargo +nightly build -p asap-driver --release

DRIVER="$(pwd)/target/release/asap-driver"
echo "Driver: $DRIVER"

echo ""
echo "=== 5. Compile test TANPA plugin (baseline) ==="
# Paksa recompile agar output benar-benar dari build ini
# (bukan biner sisa step 6 dari run sebelumnya).
touch asap-test/src/main.rs
cargo +nightly run -p asap-test
echo ""
echo "(Di atas: normal behavior, drop di akhir scope)"

echo ""
echo "=== 6. Compile test DENGAN plugin ==="
# Paksa recompile: tanpa ini cargo bisa menganggap crate masih fresh
# dan driver tidak pernah dijalankan (khususnya karena RUSTC_WRAPPER
# saja tidak selalu menginvalidasi cache).
touch asap-test/src/main.rs
RUSTC_WRAPPER="$DRIVER" cargo +nightly run -p asap-test
echo ""
echo "(Di atas: ASAP behavior, drop setelah last use)"

echo ""
echo "=== 7. Lihat MIR diff ==="
echo "--- MIR tanpa plugin ---"
RUSTFLAGS="--emit=mir -C opt-level=0" cargo +nightly build -p asap-test 2>/dev/null
echo ""
echo "--- MIR dengan plugin ---"
RUSTC_WRAPPER="$DRIVER" RUSTFLAGS="--emit=mir -C opt-level=0" cargo +nightly build -p asap-test 2>/dev/null
echo ""
echo "MIR files: $(ls target/debug/deps/asap_test-*.mir 2>/dev/null | head -2)"
