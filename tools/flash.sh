#!/usr/bin/env bash
#
# Build and flash a port, with the board looked up by name.
#
#   tools/flash.sh esp32c6-wren            # the default board, `c6`
#   tools/flash.sh esp32c6-wren c6         # or name one
#   tools/flash.sh esp32c6-wren --monitor  # and stay attached
#
# **Why this exists rather than `idf.py -p /dev/ttyACM<n>`.** The numbering
# shuffles between plug-ins, and this workstation carries a live lightning
# monitor and four moisture devices on the same hubs. `tools/board.sh` refuses
# anything flagged `forbidden` in `devices.list` by MAC, so a flash cannot land
# on somebody else's board because a port was renumbered.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck source=board.sh
source "$HERE/board.sh"

PORT_NAME="${1:-}"
if [[ -z "$PORT_NAME" ]]; then
    echo "usage: $0 <port> [board] [--monitor]" >&2
    echo >&2
    echo "ports:" >&2
    for dir in "$ROOT"/ports/*/; do
        [[ -f "$dir/CMakeLists.txt" ]] || continue
        printf '    %s\n' "$(basename "$dir")" >&2
    done
    exit 2
fi
shift

BOARD=""
MONITOR=0
for arg in "$@"; do
    case "$arg" in
        --monitor) MONITOR=1 ;;
        *) BOARD="$arg" ;;
    esac
done

PORT_DIR="$ROOT/ports/$PORT_NAME"
[[ -d "$PORT_DIR" ]] || { echo "no such port: $PORT_NAME" >&2; exit 1; }

MAC="$(board_mac "$BOARD")" || { echo "no board named '${BOARD:-<default>}' in devices.list" >&2; exit 1; }
if board_forbidden "$BOARD"; then
    echo "REFUSED: $(board_forbidden_why "$BOARD")" >&2
    exit 1
fi

DEVICE="/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_${MAC}-if00"
[[ -e "$DEVICE" ]] || { echo "board ${BOARD:-<default>} ($MAC) is not plugged in" >&2; exit 1; }

IDF="${IDF_EXPORT:-$HOME/.espressif/esp-idf/v5.5/export.sh}"
[[ -f "$IDF" ]] || { echo "no ESP-IDF at $IDF -- set IDF_EXPORT" >&2; exit 1; }

echo "port  : $PORT_NAME"
echo "board : ${BOARD:-<default>}  $MAC"

cd "$PORT_DIR"
# shellcheck disable=SC1090
source "$IDF" >/dev/null 2>&1
if (( MONITOR )); then
    idf.py -p "$DEVICE" flash monitor
else
    idf.py -p "$DEVICE" flash
fi
