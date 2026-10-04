# ASAP Drop

`asap!(expr)` wraps a value in `AsapOwned<T>` as a **marker** so the
MIR pass (`asap-driver`) can detect variables that may be dropped
as soon as possible (after the *last use* / after borrows end) instead of
at the end of the scope.

Workspace layout:

- `asap-macro/` — proc macro `asap!(expr)` → `::asap_runtime::AsapOwned::new(expr)`
- `asap-runtime/` — marker type `AsapOwned<T>` (repr transparent, `Deref`/`DerefMut` into `T`)
- `asap-driver/` — `rustc` replacement binary, injects the ASAP analysis via `RUSTC_WRAPPER`
- `asap-test/` — demo + tests (`Tracked`, asap/non-asap mix, borrows, Vec, Mutex guard)

## Requirements

- `nightly` toolchain + the `rustc-dev` and `llvm-tools-preview` components:

```bash
rustup toolchain install nightly --component rustc-dev llvm-tools-preview
```

The active toolchain is already pinned in `rust-toolchain.toml` (`channel = "nightly"`).

## Quick start (recommended)

```bash
bash build.sh
```

What `build.sh` does:

1. Installs any missing nightly components.
2. `cargo +nightly build -p asap-macro --release`
3. `cargo +nightly build -p asap-runtime --release`
4. `cargo +nightly build -p asap-driver --release`
5. Baseline without the plugin: `cargo +nightly run -p asap-test`
6. With the plugin: `RUSTC_WRAPPER="$DRIVER" cargo +nightly run -p asap-test`
7. Emits MIR for manual inspection (`.mir` files under `target/debug/deps/`).

`build.sh` succeeds = exit code `0`.

## Manual usage

```bash
# 1. Build everything
cargo +nightly build -p asap-macro --release
cargo +nightly build -p asap-runtime --release
cargo +nightly build -p asap-driver --release

# 2. Baseline (no plugin)
cargo +nightly run -p asap-test

# 3. With the plugin
DRIVER="$(pwd)/target/release/asap-driver"
RUSTC_WRAPPER="$DRIVER" cargo +nightly run -p asap-test
```

Note: cargo may consider the crate *fresh* and skip re-running the driver.
Changing `RUSTC_WRAPPER` alone does not always invalidate the cache — if
you see no `[asap]` logs and no `Compiling <crate>` line, you are running
a stale binary. Force a rebuild:

```bash
touch asap-test/src/main.rs
RUSTC_WRAPPER="$DRIVER" cargo +nightly build -p asap-test
```

Or use the helper (builds the driver, forces a recompile, then runs):

```bash
./run-asap.sh                  # project in current dir, debug
./run-asap.sh --release        # project in current dir, release
./run-asap.sh -p tes           # tes crate in workspace, debug
./run-asap.sh -p tes --release # tes crate in workspace, release
```

`[asap] Processing ...` and `[asap] Found candidate ...` logs go to stderr.

## Using it in your own project

`Cargo.toml`:

```toml
[dependencies]
asap-macro   = { path = "../asap/asap-macro" }
asap-runtime = { path = "../asap/asap-runtime" }
```

Code:

```rust
use asap_macro::asap;

fn main() {
    let a = asap!(vec![10, 20, 30]);
    a.push(40);              // DerefMut into Vec, feels like a plain Vec
    let sum: i32 = a.iter().sum();
    // last use of a above

    let b = vec![1, 2, 3];   // plain value, untouched by ASAP
    println!("{}", sum + b.iter().sum::<i32>());
}
```

Build with the driver:

```bash
RUSTC_WRAPPER="/path/to/asap-driver" cargo +nightly build
RUSTC_WRAPPER="/path/to/asap-driver" cargo +nightly run
```

Normal borrow rules still apply: while an `&*a` borrow is alive,
the drop is delayed until the borrow ends.

Patterns covered by `asap-test`:

- `test_mix` — mixes `let normal` and `let early = asap!(...)` in one scope
- `test_borrow_delays_drop` — `let r = &*a;` delays the drop
- `test_chained` — several asap variables, each dropped independently
- `test_with_vec` — `asap!(vec![...])`
- `test_mutex_early_unlock` — `asap!(data.lock().unwrap())`

## Current status / limitations

- The driver **builds and runs on nightly 1.97** and performs a real
  early-drop transform: it wraps the `mir_built` query via
  `override_queries`, finds `AsapOwned<T>` locals (visible as
  `[asap] Found candidate` / `[asap] early-drop ...` logs), splits the
  block after the last use, inserts a `Drop` terminator, and neutralizes
  the old scope drop (`Drop` → `Goto`, so no double-drop).
- Verified on `asap-test`: with the plugin, `DROP early` happens right
  after its last `USE`, before `--- middle ---`; borrows delay the drop
  until the borrow's last use; each chained variable drops independently.
- Safety design (fail-safe, never silent UB):
  - The transform runs on `mir_built`, **before borrowck**, so an unsound
    placement is rejected by borrowck as a compile error instead of
    miscompiling silently.
  - Taint analysis tracks values derived from a candidate through
    `&*a`, `Deref::deref` call chains, and reborrows; every `FakeRead`,
    `PlaceMention`, intrinsic, and asm operand counts as a use.
  - Guards: all uses must be CFG-ordered before the drop point
    (`Location::is_predecessor_of`), the drop point must reach the old
    scope drop, and move-outs (`into_inner`, returning the value) are
    skipped. Doubtful cases (loops/branches with unclear order) fall back
    to the normal end-of-scope drop.
- Known limitations:
  - Coroutine suspension points (`Yield`) are deliberately skipped.
  - Incremental compilation is disabled by the driver
    (`[asap] note: disabling incremental compilation`) so the query
    override runs on every build.
  - Storing a borrow into an escaping container (global, returned closure)
    relies on borrowck to reject it; if borrowck accepts, the drop is
    sound by construction of NLL lifetimes.
