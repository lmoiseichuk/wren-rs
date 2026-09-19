#!/usr/bin/env bash
#
# Build a named variant of a port and keep the image the numbers came from.
#
#   tools/release.sh esp32c6-wren size     # -Os, the published footprint
#   tools/release.sh esp32c6-wren perf     # -O2, the speed ceiling
#   tools/release.sh esp32c6-wren tests    # -Os + upstream's api fixtures
#
# **A number without the binary that produced it is an anecdote.** Six months
# from now "274 KB, 83 KB resident, 752 ms" is only checkable if that exact
# image is still here beside the commit it was built from -- and a rebuild from
# "the same" source is not the same binary once a toolchain has moved
# underneath it. So a release is the artefacts plus a stamp, committed together.
#
# Each variant lands in `ports/<port>/release/<variant>/` with a `VERSION` that
# carries the `esptool.py` line to flash it with no toolchain at all. That is
# what lets somebody else reproduce a measurement, or just try the thing.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

PORT_NAME="${1:-}"
VARIANT="${2:-size}"

usage() {
    echo "usage: $0 <port> [variant]" >&2
    echo >&2
    echo "ports:" >&2
    for dir in "$ROOT"/ports/*/; do
        [[ -f "$dir/CMakeLists.txt" ]] || continue
        printf '    %s\n' "$(basename "$dir")" >&2
    done
    echo >&2
    echo "variants:" >&2
    echo "    size    -Os, no test fixtures -- the published footprint" >&2
    echo "    perf    -O2, no test fixtures -- the speed ceiling" >&2
    echo "    tests   -Os plus upstream's api fixtures -- for the suite" >&2
    exit 2
}
[[ -n "$PORT_NAME" ]] || usage

PORT_DIR="$ROOT/ports/$PORT_NAME"
[[ -d "$PORT_DIR" ]] || { echo "no such port: $PORT_NAME" >&2; exit 1; }

# **The fixtures are upstream's *test* code, and image size is a published
# number.** Carrying them in a build whose footprint is quoted would report the
# harness as part of the VM, so only the `tests` variant includes them.
case "$VARIANT" in
    size)  CONFIGS="sdkconfig.defaults;sdkconfig.size"; API=OFF ;;
    perf)  CONFIGS="sdkconfig.defaults;sdkconfig.perf"; API=OFF ;;
    tests) CONFIGS="sdkconfig.defaults;sdkconfig.size"; API=ON  ;;
    *)     echo "unknown variant: $VARIANT" >&2; usage ;;
esac

IDF="${IDF_EXPORT:-$HOME/.espressif/esp-idf/v5.5/export.sh}"
[[ -f "$IDF" ]] || { echo "no ESP-IDF at $IDF -- set IDF_EXPORT" >&2; exit 1; }

COMMIT="$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
DIRTY=""
if [[ -n "$(git -C "$ROOT" status --porcelain -- ':!ports/*/release' ':!doc' 2>/dev/null)" ]]; then
    DIRTY=" (working tree dirty)"
fi
WREN_COMMIT="$(git -C "$ROOT/vendor/wren" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
BUILT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

cd "$PORT_DIR"
# shellcheck disable=SC1090
source "$IDF" >/dev/null 2>&1
IDF_VERSION="$(idf.py --version 2>/dev/null | tail -1)"

# A separate build directory per variant, so switching between them does not
# mean a full rebuild each time -- and so a stale sdkconfig cannot leak across.
BUILD_DIR="build-$VARIANT"
idf.py -B "$BUILD_DIR" -DSDKCONFIG_DEFAULTS="$CONFIGS" -DWREN_API_TESTS="$API" \
       -DSDKCONFIG="$BUILD_DIR/sdkconfig" build >/dev/null

RELEASE="$PORT_DIR/release/$VARIANT"
mkdir -p "$RELEASE"
for artefact in "$BUILD_DIR"/*.bin "$BUILD_DIR"/bootloader/bootloader.bin \
                "$BUILD_DIR"/partition_table/partition-table.bin; do
    [[ -e "$artefact" ]] && cp "$artefact" "$RELEASE/"
done

APP_BIN="$(ls "$RELEASE"/*.bin | grep -v -e bootloader -e partition | head -1)"
APP_SIZE="$(stat -c %s "$APP_BIN")"

cat > "$RELEASE/VERSION" <<EOF
port      : $PORT_NAME
variant   : $VARIANT
commit    : $COMMIT$DIRTY
wren      : $WREN_COMMIT (vendor/wren, unmodified)
idf       : $IDF_VERSION
sdkconfig : $CONFIGS
api tests : $API
built     : $BUILT
app size  : $APP_SIZE bytes
image     : $(basename "$APP_BIN")

# Flash this exact image with no toolchain and no build:
#
#   esptool.py -p <by-id path> write_flash \\
#       0x0     bootloader.bin \\
#       0x8000  partition-table.bin \\
#       0x10000 $(basename "$APP_BIN")
#
# Then open the console at 115200 and type \`.help\`.
EOF

printf '%s: %s bytes\n' "$VARIANT" "$APP_SIZE"
echo "released: ports/$PORT_NAME/release/$VARIANT"
