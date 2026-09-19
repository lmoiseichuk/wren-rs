#!/usr/bin/env bash
#
# Install everything this project needs, on Ubuntu or Debian.
#
#   tools/dependencies.sh            # say what is missing, install nothing
#   tools/dependencies.sh --install  # actually install it
#   tools/dependencies.sh --install --rust   # ...including the Rust toolchain
#
# **Checking first, installing second.** A script that starts by running `apt
# install` as its opening move is one people are right to be wary of, and most
# of the time the answer is that three of eleven things are missing. So the
# default is a report, and installing is a choice.
#
# Everything here was needed by this bench in the order it is listed. Where a
# step is not in Espressif's own instructions, a comment says why leaving it out
# failed.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

INSTALL=0
WANT_RUST=0
for arg in "$@"; do
    case "$arg" in
        --install) INSTALL=1 ;;
        --rust) WANT_RUST=1 ;;
        -h|--help) sed -n '2,12p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

IDF_VERSION="v5.5"
IDF_ROOT="${IDF_ROOT:-$HOME/.espressif/esp-idf/$IDF_VERSION}"
TARGET="esp32c6"

missing=0
note() { printf '  %-42s %s\n' "$1" "$2"; }
ok()   { note "$1" "ok"; }
gap()  { note "$1" "MISSING -- $2"; missing=$((missing + 1)); }

# --- system packages --------------------------------------------------------
#
# ESP-IDF's prerequisites, plus `python3-serial`, which is not on their list but
# which every tool in tools/ needs: the suite runner, the benchmark runner and
# the console all talk to the board over pyserial.
PACKAGES=(git wget flex bison gperf python3 python3-pip python3-venv
          cmake ninja-build ccache libffi-dev libssl-dev dfu-util
          libusb-1.0-0 python3-serial)

echo "system packages"
apt_missing=()
for package in "${PACKAGES[@]}"; do
    if dpkg -s "$package" >/dev/null 2>&1; then
        ok "$package"
    else
        gap "$package" "apt"
        apt_missing+=("$package")
    fi
done

if (( ${#apt_missing[@]} )) && (( INSTALL )); then
    echo
    echo "installing: ${apt_missing[*]}"
    sudo apt update
    sudo apt install -y "${apt_missing[@]}"
fi

# --- serial access ----------------------------------------------------------
#
# Without this every tool fails with a permission error on /dev/ttyACM*, which
# reads as a board that is not plugged in.
echo
echo "serial access"
if id -nG "$USER" | tr ' ' '\n' | grep -qx dialout; then
    ok "$USER is in the dialout group"
else
    gap "$USER is in the dialout group" "usermod"
    if (( INSTALL )); then
        sudo usermod -aG dialout "$USER"
        echo "  added -- log out and back in for it to take effect"
    fi
fi

# --- the submodule ----------------------------------------------------------
#
# The ports glob vendor/wren's sources directly, so an un-initialised submodule
# fails as a missing source directory rather than as a missing submodule.
echo
echo "this repository"
if [[ -f "$ROOT/vendor/wren/src/vm/wren_vm.c" ]]; then
    ok "vendor/wren checked out"
else
    gap "vendor/wren checked out" "git submodule"
    if (( INSTALL )); then
        git -C "$ROOT" submodule update --init --recursive
    fi
fi

if [[ -f "$ROOT/devices.list" ]]; then
    ok "devices.list"
else
    gap "devices.list" "copy devices.list.example and add your board's MAC"
    if (( INSTALL )); then
        cp "$ROOT/devices.list.example" "$ROOT/devices.list"
        echo "  copied -- now put your board's MAC in it"
    fi
fi

# --- ESP-IDF ----------------------------------------------------------------
echo
echo "ESP-IDF $IDF_VERSION"
if [[ -f "$IDF_ROOT/export.sh" ]]; then
    ok "checked out at $IDF_ROOT"
else
    gap "checked out at $IDF_ROOT" "git clone"
    if (( INSTALL )); then
        mkdir -p "$(dirname "$IDF_ROOT")"
        git clone -b "$IDF_VERSION" --recursive \
            https://github.com/espressif/esp-idf.git "$IDF_ROOT"
        (cd "$IDF_ROOT" && ./install.sh "$TARGET")
    fi
fi

# **The export has to be tried, not inferred from the directory existing.**
#
# A tree missing any single tool refuses to export at all, even a tool no build
# uses. This bench had everything except `openocd-esp32` -- a debugger -- and
# `export.sh` failed with `Activation script failed`, which says nothing about
# which tool or why. Running it is the only way to know.
if [[ -f "$IDF_ROOT/export.sh" ]]; then
    if bash -c "source '$IDF_ROOT/export.sh' >/dev/null 2>&1 && command -v idf.py >/dev/null"; then
        ok "export.sh works"
    else
        gap "export.sh works" "a tool is missing; run $IDF_ROOT/install.sh $TARGET"
        if (( INSTALL )); then
            (cd "$IDF_ROOT" && ./install.sh "$TARGET")
        fi
    fi
fi

# --- Rust, for the wren-rs port --------------------------------------------
if (( WANT_RUST )); then
    echo
    echo "Rust (for ports/esp32c6-wren-rs)"
    if command -v cargo >/dev/null 2>&1; then
        ok "cargo"
        # The C6 is RISC-V, so the stock target is enough. No espup, no xtensa
        # toolchain -- that layer only exists for Xtensa parts.
        if rustup target list --installed 2>/dev/null | grep -qx riscv32imac-esp-espidf; then
            ok "riscv32imac-esp-espidf target"
        else
            gap "riscv32imac-esp-espidf target" "rustup target add"
            (( INSTALL )) && rustup target add riscv32imac-esp-espidf
        fi
        for tool in ldproxy espflash; do
            if command -v "$tool" >/dev/null 2>&1; then
                ok "$tool"
            else
                gap "$tool" "cargo install $tool"
                (( INSTALL )) && cargo install "$tool"
            fi
        done
    else
        gap "cargo" "https://rustup.rs"
        if (( INSTALL )); then
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
            echo "  installed -- open a new shell, then run this again"
        fi
    fi
fi

echo
if (( missing == 0 )); then
    echo "everything is here."
    echo
    echo "  tools/power.sh list                   # the board should say 'present'"
    echo "  tools/flash.sh esp32c6-wren size      # build and flash"
elif (( INSTALL )); then
    echo "$missing were missing and have been installed. Run again to confirm."
else
    echo "$missing missing. Re-run with --install to fix them."
    exit 1
fi
