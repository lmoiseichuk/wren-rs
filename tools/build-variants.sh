#!/usr/bin/env bash
#
# Build every binary variant this repository can produce, into `try/`.
#
#   tools/build-variants.sh              # everything
#   tools/build-variants.sh esp32c6      # one target
#   tools/build-variants.sh x86
#
# **Why a folder of variants at all.** This crate has three numeric modes, a
# core that can be tailored to one program, a compiler that can be left out and
# two optimisation profiles, and the interesting question is nearly always what
# one of those costs against another. Reading that off a table someone typed is
# how a table goes stale; building them all and measuring each is how it does
# not. Every size in `try/README.md` is measured at build time.
#
# The result is self-contained: binaries, a stamp saying what each is, a README
# and `try/flash.sh`. Nothing in it is committed -- `try/` is a scratch folder,
# rebuilt whenever the question comes up again.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
OUT="$ROOT/try"
COMMIT="$(git -C "$ROOT" rev-parse --short=12 HEAD)"
DIRTY=""
git -C "$ROOT" diff --quiet || DIRTY=" (working tree dirty)"

WANT="${1:-all}"

# name | port | bin | profile | cargo flags | what it is
#
# `--no-default-features` on the wrenc port selects the tailored core; without
# it the port's own default pulls `core_full` in for the benchmark runner.
ESP32C6=(
  "bench-f64-speed|esp32c6-wren-rs||speed||Full Wren, doubles, compiler on the device. The benchmark runner, and the build every published speed number comes from."
  "bench-f64-size|esp32c6-wren-rs||size||The same at -Os, which is what a firmware with other things to do would use."
  "bench-f32-speed|esp32c6-wren-rs||speed|--features f32|Singles instead of doubles. Faster and smaller, and not Wren: exact on integers only to 2^24."
  "bench-nofp-speed|esp32c6-wren-rs||speed|--features nofp|Integers only. The fastest and smallest of the three, with no fractions at all."
  "wrenc-f64-size|esp32c6-wrenc-rs|esp32c6-wrenc-rs|size||Bytecode only, no compiler linked. What a device runs when a workstation did the parsing."
  "uwren-tailored-size|esp32c6-wrenc-rs|uwren|size|--no-default-features --features census|The small one: integers, no compiler, only the core methods fib's manifest asks for, and that core frozen into the image."
  "uwren-fullcore-size|esp32c6-wrenc-rs|uwren|size||The same program carrying the whole core library, so the tailoring can be priced."
  "boot-default|esp32c6-wren-boot||size||The smallest thing that starts a VM at all."
  "boot-compiler|esp32c6-wren-boot||size|--features compiler|The same, with the compiler linked, which is what the compiler costs in flash."
)

# Every benchmark, both ways. The tailored column is the interesting one: each
# gets a core generated from its own manifest, so they are not the same build
# with a different payload -- `binary_trees` needs classes and fields that
# `fib` never mentions, and its frozen core is correspondingly larger.
BENCHMARKS=(binary_trees fib list_build method_call)
X86_MODES=(
  "full|Full Wren as a host executable, with the program compiled in as bytecode."
  "uwren|Tailored: integers, no compiler, only the core the program's manifest asks for, frozen into the image."
)

stamp() {  # dir name what features profile artifact
    cat >"$1/VERSION" <<EOF
variant : $2
commit  : $COMMIT$DIRTY
built   : $(date -u +%Y-%m-%dT%H:%M:%SZ)
profile : $5
flags   : ${4:-(none)}
file    : $(basename "$6")
bytes   : $(stat -c%s "$6")
what    : $3
EOF
}

mkdir -p "$OUT"
FAILED=()

if [[ "$WANT" == "all" || "$WANT" == "esp32c6" ]]; then
    echo "=== esp32c6 ==="
    for row in "${ESP32C6[@]}"; do
        IFS='|' read -r name port bin profile flags what <<<"$row"
        printf '  %-24s ' "$name"
        dir="$OUT/esp32c6/$name"
        mkdir -p "$dir"
        build=(cargo build --profile "$profile")
        [[ -n "$bin" ]] && build+=(--bin "$bin")
        # shellcheck disable=SC2206
        [[ -n "$flags" ]] && build+=($flags)
        if ! ( cd "$ROOT/ports/$port" && "${build[@]}" ) >"$dir/build.log" 2>&1; then
            echo "FAILED -- $dir/build.log"
            FAILED+=("$name")
            continue
        fi
        elf="$ROOT/ports/$port/target/riscv32imac-unknown-none-elf/$profile/${bin:-$port}"
        if [[ ! -f "$elf" ]]; then
            echo "FAILED -- no artifact at $elf"
            FAILED+=("$name")
            continue
        fi
        cp "$elf" "$dir/"
        # **The flashable image, not the ELF.** An ELF carries symbols and
        # section headers a board never sees; the image is what the partition
        # has to hold, and it needs no board to produce.
        espflash save-image --chip esp32c6 "$elf" "$dir/$(basename "$elf").bin" >/dev/null 2>&1
        stamp "$dir" "$name" "$what" "$flags" "$profile" "$dir/$(basename "$elf")"
        printf 'elf %8d B   image %8d B\n' \
            "$(stat -c%s "$dir/$(basename "$elf")")" \
            "$(stat -c%s "$dir/$(basename "$elf").bin" 2>/dev/null || echo 0)"
    done
fi

# **A tailored image is per program, so there is one per benchmark.** The core
# is generated from that program's own manifest, and the generated bin is a
# copy of `uwren.rs` with its three fib-specific references swapped -- the
# committed port is left exactly as it is.
if [[ "$WANT" == "all" || "$WANT" == "esp32c6" ]]; then
    echo "=== esp32c6, tailored per benchmark ==="
    PORT="$ROOT/ports/esp32c6-wrenc-rs"
    # The freezer must run with the features the firmware is built with, or
    # every method entry indexes the wrong primitive. See doc/wren_native.md.
    cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml"         --no-default-features --features "std,nofp" --example freeze 2>/dev/null
    FREEZE="$ROOT/target/release/examples/freeze"
    for program in "${BENCHMARKS[@]}"; do
        name="uwren-$program-size"
        printf '  %-24s ' "$name"
        dir="$OUT/esp32c6/$name"
        mkdir -p "$dir"
        core="$PORT/src/bin/generated_core.rs"
        gen="$PORT/src/bin/generated_uwren.rs"
        if ! "$FREEZE" "$ROOT/benchmarks/wrenc/$program.wrenc" >"$core" 2>"$dir/build.log"; then
            echo "FAILED to freeze -- $dir/build.log"; FAILED+=("$name"); rm -f "$core"; continue
        fi
        sed -e 's/"fib_core\.rs"/"generated_core.rs"/'             -e 's/\bfib_core\b/generated_core/g'             -e "s#/fib\.wrenc#/$program.wrenc#"             "$PORT/src/bin/uwren.rs" >"$gen"
        if ( cd "$PORT" && cargo build --profile size --bin generated_uwren                 --no-default-features --features census ) >>"$dir/build.log" 2>&1; then
            elf="$PORT/target/riscv32imac-unknown-none-elf/size/generated_uwren"
            cp "$elf" "$dir/$program"
            espflash save-image --chip esp32c6 "$elf" "$dir/$program.bin" >/dev/null 2>&1
            stamp "$dir" "$name" "Tailored image for $program: integers, no compiler, only the core its manifest asks for, frozen into flash."                 "--no-default-features --features census" "size" "$dir/$program"
            printf 'elf %8d B   image %8d B   core %6d B of Rust\n'                 "$(stat -c%s "$dir/$program")"                 "$(stat -c%s "$dir/$program.bin" 2>/dev/null || echo 0)"                 "$(stat -c%s "$core")"
        else
            echo "FAILED -- $dir/build.log"; FAILED+=("$name")
        fi
        rm -f "$core" "$gen"
    done
fi

if [[ "$WANT" == "all" || "$WANT" == "x86" ]]; then
    echo "=== x86 ==="
    for program in "${BENCHMARKS[@]}"; do
        for row in "${X86_MODES[@]}"; do
            IFS='|' read -r mode what <<<"$row"
            name="$program-$mode"
            printf '  %-24s ' "$name"
            dir="$OUT/x86/$name"
            mkdir -p "$dir"
            args=("$ROOT/benchmarks/wren/$program.wren" --out "$dir/build")
            [[ "$mode" == "uwren" ]] && args+=(--uwren)
            if ! "$ROOT/tools/make_native_executable.sh" "${args[@]}" >"$dir/build.log" 2>&1; then
                echo "FAILED -- $dir/build.log"
                FAILED+=("$name")
                continue
            fi
            exe="$dir/build/$program/target/release/$program"
            cp "$exe" "$dir/$program"
            stamp "$dir" "$name" "$what" "--$mode" "release" "$dir/$program"
            printf 'executable %8d B\n' "$(stat -c%s "$dir/$program")"
        done
    done
fi

echo
if (( ${#FAILED[@]} )); then
    echo "failed: ${FAILED[*]}"
else
    echo "all variants built"
fi
"$HERE/write-variants-readme.sh"
