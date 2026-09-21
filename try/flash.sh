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
