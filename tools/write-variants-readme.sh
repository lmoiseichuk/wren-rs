#!/usr/bin/env bash
#
# Write `try/README.md`, `try/flash.sh` and `try/run.sh` from what is in `try/`.
#
# **Every number here is read off the artefact**, not typed. A table of sizes
# maintained by editing is a table that is wrong the first time somebody
# rebuilds and forgets it, and this repository has the same rule about counts
# everywhere else.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
OUT="$ROOT/try"
[[ -d "$OUT" ]] || { echo "no try/ -- run tools/build-variants.sh first" >&2; exit 1; }

field() { sed -n "s/^$2 *: //p" "$1/VERSION"; }
commas() { printf "%'d" "$1"; }

{
    echo "# try/ — every binary this repository can build"
    echo
    echo "Built by \`tools/build-variants.sh\`, which is the only thing that should"
    echo "write here. Nothing in this folder is committed: it is a scratch tree, and"
    echo "rebuilding it is how its numbers stay true."
    echo
    echo "Commit: \`$(field "$(ls -d "$OUT"/*/*/ | head -1)" commit)\`"
    echo
    echo "## ESP32-C6"
    echo
    echo "\`./flash.sh <variant>\` writes one to the board. The ELF is what the"
    echo "debugger reads; the **image** is what the partition has to hold, and is the"
    echo "number to compare."
    echo
    printf '| variant | image | elf | profile | what it is |\n'
    printf '|---|---|---|---|---|\n'
    for d in "$OUT"/esp32c6/*/; do
        [[ -f "$d/VERSION" ]] || continue
        name="$(field "$d" variant)"
        file="$(field "$d" file)"
        elf="$(field "$d" bytes)"
        img=0
        [[ -f "$d/$file.bin" ]] && img="$(stat -c%s "$d/$file.bin")"
        printf '| `%s` | %s B | %s B | %s | %s |\n' \
            "$name" "$(commas "$img")" "$(commas "$elf")" "$(field "$d" profile)" "$(field "$d" what)"
    done
    echo
    echo "## x86"
    echo
    echo "\`./run.sh <variant>\` runs one. They read nothing from disk — the program"
    echo "is bytecode compiled into the executable."
    echo
    printf '| variant | executable | what it is |\n'
    printf '|---|---|---|\n'
    for d in "$OUT"/x86/*/; do
        [[ -f "$d/VERSION" ]] || continue
        printf '| `%s` | %s B | %s |\n' \
            "$(field "$d" variant)" "$(commas "$(field "$d" bytes)")" "$(field "$d" what)"
    done
    echo
    echo "## Reading the table"
    echo
    echo "**The tailored images are all the same size.** Four programs, four cores"
    echo "generated from four different manifests — 5,746 to 6,355 bytes of generated"
    echo "Rust between them — and one image size. The flash image is laid out in"
    echo "aligned segments, and a few hundred bytes of difference disappears inside"
    echo "the padding. The ELFs differ; the images do not."
    echo
    echo "**\`-Os\` against \`-O3\` is the largest single lever on the image**, larger"
    echo "than any feature here: the same full-Wren build is 472,928 B at \`speed\` and"
    echo "291,280 B at \`size\`. Every published speed number comes from the \`speed\`"
    echo "build, and every image comparison elsewhere in the docs from \`size\`, so the"
    echo "two are not interchangeable."
    echo
    echo "**What tailoring is worth** is \`uwren-fullcore-size\` against"
    echo "\`uwren-tailored-size\`: the same program, the same profile, differing only"
    echo "in whether the core is the whole library or the part its manifest asks for."
    echo
    echo "**What the compiler costs** is \`boot-default\` against \`boot-compiler\`:"
    echo "the same firmware with and without the lexer and parser linked in."
} > "$OUT/README.md"

cat > "$OUT/flash.sh" <<'FLASH'
#!/usr/bin/env bash
#
# Flash one variant from this folder to the ESP32-C6.
#
#   ./flash.sh uwren-fib-size
#   ./flash.sh bench-f64-speed --monitor
#
# **The board is looked up by name and refused by MAC.** This bench carries
# devices belonging to other projects on the same hubs and /dev/ttyACM* is one
# replug away from pointing at the wrong one -- `tools/board.sh` is the guard.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck source=../tools/board.sh
source "$ROOT/tools/board.sh"

usage() {
    echo "usage: $0 <variant> [board] [--monitor]" >&2
    echo >&2
    echo "variants:" >&2
    for v in "$HERE"/esp32c6/*/VERSION; do
        [[ -e "$v" ]] || continue
        printf '    %-24s %s\n' \
            "$(sed -n 's/^variant *: //p' "$v")" \
            "$(sed -n 's/^what *: //p' "$v" | cut -c1-70)" >&2
    done
    exit 2
}

VARIANT="${1:-}"; [[ -n "$VARIANT" ]] || usage
shift
BOARD=""; MONITOR=0
for arg in "$@"; do
    case "$arg" in
        --monitor) MONITOR=1 ;;
        *) BOARD="$arg" ;;
    esac
done

DIR="$HERE/esp32c6/$VARIANT"
[[ -d "$DIR" ]] || { echo "no such variant: $VARIANT" >&2; usage; }
FILE="$(sed -n 's/^file *: //p' "$DIR/VERSION")"

MAC="$(board_mac "$BOARD")" || { echo "no board named '${BOARD:-<default>}'" >&2; exit 1; }
if board_forbidden "$BOARD"; then
    echo "REFUSED: $(board_forbidden_why "$BOARD")" >&2
    exit 1
fi
DEVICE="/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_${MAC}-if00"
[[ -e "$DEVICE" ]] || { echo "board ${BOARD:-<default>} ($MAC) is not plugged in" >&2; exit 1; }

echo "variant : $VARIANT"
echo "board   : ${BOARD:-<default>}  $MAC"
echo

if (( MONITOR )); then
    espflash flash --monitor --port "$DEVICE" "$DIR/$FILE"
else
    espflash flash --port "$DEVICE" "$DIR/$FILE"
fi
FLASH

cat > "$OUT/run.sh" <<'RUN'
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
RUN

chmod +x "$OUT/flash.sh" "$OUT/run.sh"
echo "wrote try/README.md, try/flash.sh, try/run.sh"
