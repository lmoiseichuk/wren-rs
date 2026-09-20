#!/usr/bin/env bash
#
# Build the Rust port, flash it, and capture one benchmark run.
#
#   tools/measure-rs.sh                        # the speed profile, as committed
#   tools/measure-rs.sh size                   # the footprint profile
#   tools/measure-rs.sh speed --pad 4          # ...at a chosen code placement
#   tools/measure-rs.sh speed --bin counters   # a different binary in the port
#   tools/measure-rs.sh speed --slot-block 256 # blocked slot tables, this size
#   tools/measure-rs.sh speed --features f32   # any cargo feature of the port
#   tools/measure-rs.sh --only method_call     # one benchmark, for a fast loop
#
# **Why this exists rather than `cargo run`.** The port's cargo runner is
# `espflash flash --monitor`, which never exits: it flashes, prints, and then
# holds the serial port until it is killed. A measurement wants the output and
# then the port back, so this waits for the run's own `done` line and lets go.
# A monitor left running is why a later flash says "Device or resource busy".
#
# **`--pad N` is the control, not a tuning knob.** Two builds of the same source
# differ by three to four per cent purely in where their instructions land, so a
# small difference between two source versions says nothing on its own. Building
# each at several placements holds that roughly still while the source varies.
# See doc/wren-rs/profiling.md; the committed default is four bytes.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck source=board.sh
source "$HERE/board.sh"

PORT_DIR="$ROOT/ports/esp32c6-wren-rs"
PROFILE="speed"
BOARD=""
BIN=""
PAD=""
LOG=""
FEATURES=""
SLOT_BLOCK=""

while (( $# )); do
    case "$1" in
        size|speed) PROFILE="$1" ;;
        --pad) PAD="${2:-}"; shift ;;
        --bin) BIN="${2:-}"; shift ;;
        --features) FEATURES="${2:-}"; shift ;;
        --slot-block) SLOT_BLOCK="${2:-}"; shift ;;
        --only) export WREN_ONLY="${2:-}"; shift ;;
        --log) LOG="${2:-}"; shift ;;
        --board) BOARD="${2:-}"; shift ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# **The board is looked up by name and refused by MAC**, because this bench
# carries devices belonging to other projects on the same hubs and /dev/ttyACM*
# renumbers between plug-ins.
MAC="$(board_mac "$BOARD")" || { echo "no board named '${BOARD:-<default>}' in devices.list" >&2; exit 1; }
if board_forbidden "$BOARD"; then
    echo "REFUSED: $(board_forbidden_why "$BOARD")" >&2
    exit 1
fi
DEVICE="/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_${MAC}-if00"
[[ -e "$DEVICE" ]] || { echo "board ${BOARD:-<default>} ($MAC) is not plugged in" >&2; exit 1; }

LOG="${LOG:-$(mktemp -t wren-rs-measure.XXXXXX.log)}"

cd "$PORT_DIR" || exit 1

BUILD=(cargo build --profile "$PROFILE")
[[ -n "$BIN" ]] && BUILD+=(--bin "$BIN")

# **A block size implies blocked tables.** Asking for one in a build that has
# no blocks is refused on the device by an assertion, which is a slow way to
# find out; adding the feature here is what the caller meant.
if [[ -n "$SLOT_BLOCK" ]]; then
    export WREN_SLOT_BLOCK="$SLOT_BLOCK"
    [[ ",$FEATURES," == *",blocked-slots,"* ]] || FEATURES="${FEATURES:+$FEATURES,}blocked-slots"
    echo "slot block: $SLOT_BLOCK slots"
fi
[[ -n "$FEATURES" ]] && BUILD+=(--features "$FEATURES")

# **A placement override replaces the whole flag list, it does not add to it.**
# `RUSTFLAGS` wins over `.cargo/config.toml` outright rather than merging, so
# the linker arguments have to be repeated here or the link fails on the memory
# map. They are the same two the config carries.
if [[ -n "$PAD" ]]; then
    case "$PAD" in
        0) EXPONENT="" ;;
        2) EXPONENT=1 ;;
        4) EXPONENT=2 ;;
        8) EXPONENT=3 ;;
        16) EXPONENT=4 ;;
        32) EXPONENT=5 ;;
        *) echo "--pad takes a power of two from 0 to 32" >&2; exit 2 ;;
    esac
    FLAGS="-Clink-arg=-Tlinkall.x -Clink-arg=--undefined=esp_app_desc"
    [[ -n "$EXPONENT" ]] && FLAGS="$FLAGS -Cllvm-args=-align-all-nofallthru-blocks=$EXPONENT"
    export RUSTFLAGS="$FLAGS"
    echo "placement : branch targets padded to ${PAD} B"
fi

echo "profile   : $PROFILE"
echo "board     : ${BOARD:-<default>}  $MAC"
"${BUILD[@]}" >/dev/null 2>&1 || { echo "build failed" >&2; exit 1; }

IMAGE="target/riscv32imac-unknown-none-elf/$PROFILE/${BIN:-esp32c6-wren-rs}"
: > "$LOG"
espflash flash --monitor --non-interactive --port "$DEVICE" "$IMAGE" >> "$LOG" 2>&1 &
FLASH=$!

# The benchmark set is about half a minute at `-O3` and twice that at `-Os`;
# the ceiling is generous because a failed run should report rather than hang.
for _ in $(seq 1 300); do
    grep -qE "^done|Error:|PANIC" "$LOG" && break
    sleep 2
done
kill $FLASH 2>/dev/null
wait $FLASH 2>/dev/null

echo
# The flashing tool and the ROM both talk on this port; what is wanted is
# what the firmware said.
grep -vE "^I \(|^ets |boot|esp_image|clk|mode:|load:|entry|^Flash |^Chip |Crystal|^MAC |App/|Segment|Erasing|Writing|Uploading|Connecting|INFO|^\[20|^ESP-ROM|^Build:|^Saved PC|^SPIWP|^Features|^Partition|^Serial |^Product|^Cores|^Stub|^Changing|^Detected|^Using|^$" "$LOG"
echo
echo "full output: $LOG"
