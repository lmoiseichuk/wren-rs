#!/usr/bin/env bash
#
# Compile every `.wren` this repository ships as bytecode, and check the result.
#
#   tools/build-bytecode.sh           # rebuild doc/examples/ and benchmarks/wrenc/
#   tools/build-bytecode.sh --check   # verify they match their sources, build nothing
#
# **A `.wrenc` in the tree is a build artefact that looks like a source file.**
# It is committed because the ports `include_bytes!` it and a firmware build
# should not need a host compiler; but committed bytecode that has drifted from
# the `.wren` beside it is worse than none, because both files look current.
#
# Every `.wrenc` carries a SHA-256 of the source it came from, which is exactly
# what makes `--check` possible: it re-hashes the `.wren` and compares. Run the
# check in CI and drift is caught by the build rather than by a device
# behaving like a version of the program nobody can find.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

CHECK=0
[[ "${1:-}" == "--check" ]] && CHECK=1

# Built once, then reused: `cargo run` re-checks the whole crate per invocation
# and there are a dozen files to get through.
cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" --example wrenc
WRENC="$ROOT/target/release/examples/wrenc"

# Where bytecode is kept, and which sources it comes from. The benchmark blobs
# live apart from their sources because `benchmarks/wren/` is glob-run by the
# suite and a stray `.wrenc` there would be picked up as a program.
compile_into() {
    local source_dir="$1" output_dir="$2"
    mkdir -p "$output_dir"
    for source in "$source_dir"/*.wren; do
        [[ -e "$source" ]] || continue
        local name output
        name="$(basename "$source" .wren)"
        output="$output_dir/$name.wrenc"

        if (( CHECK )); then
            if [[ ! -f "$output" ]]; then
                echo "MISSING  $output" >&2
                failures=$((failures + 1))
                continue
            fi
            # `--check` re-hashes the source and compares it with the stamp.
            if "$WRENC" --check "$source" "$output" >/dev/null 2>&1; then
                printf '  ok      %s\n' "${output#"$ROOT"/}"
            else
                echo "STALE    ${output#"$ROOT"/} -- rebuild with $0" >&2
                failures=$((failures + 1))
            fi
        else
            "$WRENC" "$source" "$output"
        fi
    done
}

failures=0
compile_into "$ROOT/doc/examples" "$ROOT/doc/examples"
compile_into "$ROOT/benchmarks/wren" "$ROOT/benchmarks/wrenc"

if (( CHECK )); then
    if (( failures )); then
        echo >&2
        echo "$failures file(s) out of date." >&2
        exit 1
    fi
    echo "every .wrenc matches its source."
fi
