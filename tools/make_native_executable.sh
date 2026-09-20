#!/usr/bin/env bash
#
# Build a native executable that has a Wren program compiled into it.
#
#   tools/make_native_executable.sh doc/examples/boot.wren
#   tools/make_native_executable.sh benchmarks/wren/fib.wren --uwren
#   tools/make_native_executable.sh benchmarks/wren --uwren --run
#
# **Why this exists.** The interesting failures in a tailored build -- a method
# symbol numbered differently, a frozen table indexed against the wrong install
# order -- reach a device only as a wrong answer, and a flash-and-watch cycle
# is the better part of a minute. The same program built for the host runs in
# milliseconds and fails in the same ways, so this is where to iterate; the
# board is for confirming the result and measuring memory.
#
# **Two modes.** The default is full Wren: the whole core library, doubles, and
# the compiler linked in. `--uwren` is the shrunk one -- integers instead of
# doubles, no compiler, only the core methods the program's manifest asks for,
# and that core frozen into the image rather than built at start-up. It is the
# same combination `ports/esp32c6-wrenc-rs` ships, so a program that works here
# works there.
#
# See doc/wren_native.md.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

SOURCES=()
MODE="full"
OUT=""
RUN=0

usage() {
    sed -n '2,24p' "$0" | sed 's/^# \?//'
    echo
    echo "options:"
    echo "  --uwren      the shrunk build: integers, no compiler, frozen tailored core"
    echo "  --out DIR    where to build (default: target/native)"
    echo "  --run        run each executable after building it"
    exit "${1:-0}"
}

while (( $# )); do
    case "$1" in
        --uwren) MODE="uwren" ;;
        --full) MODE="full" ;;
        --out) OUT="${2:-}"; shift ;;
        --run) RUN=1 ;;
        -h|--help) usage 0 ;;
        -*) echo "unknown option: $1" >&2; usage 2 ;;
        *) SOURCES+=("$1") ;;
    esac
    shift
done

(( ${#SOURCES[@]} )) || { echo "nothing to build: name a .wren file or a directory" >&2; usage 2; }

OUT="${OUT:-$ROOT/target/native}"
mkdir -p "$OUT"

# **A directory means every `.wren` directly in it**, one executable each --
# not a recursive sweep, because a tree of Wren usually has imports and
# fixtures in it that are not programs.
EXPANDED=()
for entry in "${SOURCES[@]}"; do
    if [[ -d "$entry" ]]; then
        for source in "$entry"/*.wren; do
            [[ -e "$source" ]] && EXPANDED+=("$source")
        done
    elif [[ -f "$entry" ]]; then
        EXPANDED+=("$entry")
    else
        echo "no such file or directory: $entry" >&2
        exit 1
    fi
done
(( ${#EXPANDED[@]} )) || { echo "no .wren files found" >&2; exit 1; }

# The two feature sets, named once. `uwren` on the host is `std` for the
# runtime plus `nofp` for the numbers; it deliberately leaves out `core_full`
# and `compiler`, which is what makes the core small enough to be worth
# freezing.
case "$MODE" in
    full)  WREN_FEATURES="default" ;;
    uwren) WREN_FEATURES="std,nofp" ;;
esac

# Built once and reused; `cargo run` re-checks the whole crate per invocation.
echo "building the tools..."
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" --example wrenc
WRENC="$ROOT/target/release/examples/wrenc"
if [[ "$MODE" == "uwren" ]]; then
    cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" \
        --no-default-features --features "$WREN_FEATURES" --example freeze
    FREEZE="$ROOT/target/release/examples/freeze"
fi

for source in "${EXPANDED[@]}"; do
    name="$(basename "$source" .wren)"
    crate="$OUT/$name"
    mkdir -p "$crate/src"

    echo
    echo "=== $name ($MODE) ==="

    # 1. The program, as bytecode. The executable carries this rather than the
    #    text, which is what lets the `uwren` build leave the compiler out.
    "$WRENC" "$source" "$crate/src/program.wrenc" >/dev/null
    printf '    bytecode  %8d B\n' "$(stat -c%s "$crate/src/program.wrenc")"

    # 2. For `uwren`, the core library this program needs, as Rust.
    #
    #    **Generated with the same features it will be compiled with.** A
    #    frozen method entry holds a primitive index, and those are positions
    #    in the sequence of `define` calls that the features decide -- so a
    #    core generated one way and compiled another indexes the wrong
    #    primitive for every method, silently. `FrozenCore::disagreement`
    #    catches it at start-up; generating it here is what stops it happening.
    if [[ "$MODE" == "uwren" ]]; then
        "$FREEZE" "$crate/src/program.wrenc" > "$crate/src/frozen_core.rs"
        printf '    core      %8d B of Rust\n' "$(stat -c%s "$crate/src/frozen_core.rs")"
    fi

    # 3. A crate around it.
    if [[ "$MODE" == "uwren" ]]; then
        # The feature list is a TOML array, so the comma-separated form the
        # cargo command line takes has to be quoted item by item.
        FEATURE_ARRAY="$(echo "$WREN_FEATURES" | sed 's/[^,][^,]*/"&"/g')"
        DEP="wren = { path = \"$ROOT/crates/wren\", default-features = false, features = [$FEATURE_ARRAY] }"
    else
        DEP="wren = { path = \"$ROOT/crates/wren\" }"
    fi
    cat > "$crate/Cargo.toml" <<TOML
# Generated by tools/make_native_executable.sh -- see doc/wren_native.md.
[package]
name = "$name"
version = "0.0.0"
edition = "2021"
publish = false

[workspace]

[dependencies]
$DEP

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
TOML

    if [[ "$MODE" == "uwren" ]]; then
        cat > "$crate/src/main.rs" <<'RUST'
// Generated by tools/make_native_executable.sh -- see doc/wren_native.md.
//
// The program and its core library are both in this image: the bytecode below
// and the classes, method tables and names in `frozen_core.rs`. Nothing is read from
// disk and nothing is compiled at start-up.
mod frozen_core;

const PROGRAM: &[u8] = include_bytes!("program.wrenc");

fn main() {
    let manifest = match wren::wrenc::manifest(PROGRAM) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("the bundled program is not loadable: {}", error.message());
            std::process::exit(1);
        }
    };

    let mut vm = wren::Vm::with_frozen_core(&frozen_core::CORE, &manifest);

    // **A stale core dispatches to the wrong method and says nothing.** This
    // is the one check worth making unconditionally; see `FrozenCore`.
    if let Some(why) = frozen_core::CORE.disagreement(&vm.method_names, vm.primitives.len()) {
        eprintln!("the built-in core does not match this build: {why}");
        std::process::exit(1);
    }

    let loaded = match wren::wrenc::load(&mut vm, PROGRAM) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("cannot load: {}", error.message());
            std::process::exit(1);
        }
    };
    match vm.run_closure(loaded.closure) {
        Ok(()) => print!("{}", vm.output_str()),
        Err(error) => {
            print!("{}", vm.output_str());
            eprintln!("line {}: {}", error.line, error.message);
            std::process::exit(1);
        }
    }
}
RUST
    else
        cat > "$crate/src/main.rs" <<'RUST'
// Generated by tools/make_native_executable.sh -- see doc/wren_native.md.
//
// Full Wren: the whole core library, and the program carried as bytecode so
// that running it needs no source file beside the executable.
const PROGRAM: &[u8] = include_bytes!("program.wrenc");

fn main() {
    let mut vm = wren::Vm::new();
    let loaded = match wren::wrenc::load(&mut vm, PROGRAM) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("cannot load: {}", error.message());
            std::process::exit(1);
        }
    };
    match vm.run_closure(loaded.closure) {
        Ok(()) => print!("{}", vm.output_str()),
        Err(error) => {
            print!("{}", vm.output_str());
            eprintln!("line {}: {}", error.line, error.message);
            std::process::exit(1);
        }
    }
}
RUST
    fi

    # 4. Build it.
    if ! cargo build --quiet --release --manifest-path "$crate/Cargo.toml" 2>"$crate/build.log"; then
        echo "    BUILD FAILED -- $crate/build.log" >&2
        sed -n '1,20p' "$crate/build.log" >&2
        exit 1
    fi
    binary="$crate/target/release/$name"
    printf '    executable%8d B   %s\n' "$(stat -c%s "$binary")" "$binary"

    if (( RUN )); then
        echo "    --- output ---"
        "$binary" | sed 's/^/    /'
    fi
done
