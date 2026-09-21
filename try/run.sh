#!/usr/bin/env bash
#
# Run one x86 variant from this folder.
#
#   ./run.sh fib-uwren
#   ./run.sh binary_trees-full
#
# They take no arguments and read nothing: the program is bytecode compiled
# into the executable, and the core it needs is compiled in beside it.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
    echo "usage: $0 <variant>" >&2
    echo >&2
    echo "variants:" >&2
    for v in "$HERE"/x86/*/VERSION; do
        [[ -e "$v" ]] || continue
        printf '    %-24s %s\n' \
            "$(sed -n 's/^variant *: //p' "$v")" \
            "$(sed -n 's/^what *: //p' "$v" | cut -c1-70)" >&2
    done
    exit 2
}

VARIANT="${1:-}"; [[ -n "$VARIANT" ]] || usage
DIR="$HERE/x86/$VARIANT"
[[ -d "$DIR" ]] || { echo "no such variant: $VARIANT" >&2; usage; }
exec "$DIR/$(sed -n 's/^file *: //p' "$DIR/VERSION")"
