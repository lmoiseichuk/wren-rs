# Ports

One directory per **(board, implementation)** pair, named `<board>-<what>`. The
board comes first because that is what the three share: the whole point of this
project is comparing implementations on identical silicon, and a layout that
grouped by implementation would hide that.

| port | what it is | step |
|---|---|---|
| [`esp32c6-wren`](esp32c6-wren) | upstream Wren in C, unmodified, on the C6 | 1 — the reference |
| [`esp32c6-micropython`](esp32c6-micropython) | MicroPython on the same board | 2 — the baseline |
| [`esp32c6-wren-rs`](esp32c6-wren-rs) | this repository's Rust VM | 3 — the deliverable |

## What you need, on Ubuntu or Debian

Everything below was what this bench actually needed, in the order it needed it.
Where a step is not in Espressif's own instructions it is because leaving it out
failed here.

### System packages

ESP-IDF's prerequisites, from their install guide:

```sh
sudo apt update
sudo apt install -y git wget flex bison gperf python3 python3-pip python3-venv \
                    cmake ninja-build ccache libffi-dev libssl-dev dfu-util \
                    libusb-1.0-0 python3-serial
```

`python3-serial` is not on Espressif's list but every tool in `tools/` needs it:
the suite runner, the benchmark runner and the console all talk to the board
over `pyserial`.

### Serial port access

```sh
sudo usermod -aG dialout "$USER"     # then log out and back in
```

Without this every tool fails with a permission error on `/dev/ttyACM*`, which
looks like a missing board.

### ESP-IDF v5.5

```sh
mkdir -p ~/.espressif/esp-idf && cd ~/.espressif/esp-idf
git clone -b v5.5 --recursive https://github.com/espressif/esp-idf.git v5.5
cd v5.5 && ./install.sh esp32c6
```

**Run `install.sh`, not a partial install.** A tree missing one tool refuses to
export at all, even when that tool has nothing to do with building. This bench
had everything except `openocd-esp32` — a *debugger* — and `export.sh` failed
with `Activation script failed` until it was fetched:

```sh
~/.espressif/python_env/idf5.5_py3.13_env/bin/python \
    ~/.espressif/esp-idf/v5.5/tools/idf_tools.py install openocd-esp32
```

If IDF lives somewhere else, point the scripts at it:

```sh
export IDF_EXPORT=/path/to/esp-idf/export.sh
```

### This repository

```sh
git clone --recurse-submodules <this repo>
cd wren-rs
cp devices.list.example devices.list    # then put your board's MAC in it
```

`vendor/wren` is a submodule. Without `--recurse-submodules` the ports have
nothing to build and CMake fails on a missing source directory. An existing
clone catches up with `git submodule update --init --recursive`.

### Check it works

```sh
tools/power.sh list                      # the board should say `present`
tools/flash.sh esp32c6-wren size         # build and flash
tools/console.py <by-id path> 30         # a banner, then a `>` prompt
```

## The rule that makes the comparison mean anything

**The same board, the same clock, the same benchmark source, the same
measurement method.** A port that changes any of those produces a number that
cannot be put in the same table as the others. Where a port *must* differ — a
different console driver, a different heap size — it says so in its own README,
under a heading called "How this differs", so the caveat travels with the
number.

## What each port has to provide

Three things, so a benchmark run is the same command everywhere:

* **A console on the TTY.** Read a line, evaluate it, print the result.
* **A way to run a named benchmark** from the shared set, so the host does not
  have to paste source in over serial and time the echo.
* **A report of its own size and memory**: image size, static RAM, and free heap
  after start-up. Size is as much of the comparison as speed, and it is the part
  most easily left unmeasured because nothing fails when it grows.

## Every port keeps the image its numbers came from

`ports/<port>/release/` holds the binaries a measurement was actually taken
with, plus a `VERSION` stamp naming the commit, the `vendor/wren` commit, the
IDF version, the build time and the image size.

```
tools/release.sh esp32c6-wren
```

**A number without the binary that produced it is an anecdote.** Six months from
now "274 KB, 83 KB resident, 752 ms" is only checkable if that exact image is
still here beside the commit it was built from — and a rebuild from "the same"
source is not the same binary once a toolchain has moved underneath it. The
script refuses a dirty tree for the same reason: a stamp naming a commit the
binary was not built from is worse than no stamp, because it looks
authoritative.

The `VERSION` file also carries the `esptool.py` line that reflashes that image
with no toolchain at all, which is what makes an old measurement reproducible
rather than merely recorded.

## Upstream stays upstream

`vendor/wren` is a git submodule pinned to wren-lang/wren, and **nothing in this
repository modifies it**. A port that needs a change makes it in the port's own
files — a shim, a config header, a wrapper — never in the submodule. If a change
there turns out to be unavoidable, that is a finding worth writing down rather
than a patch worth hiding, because it means Wren as published does not build for
this class of part.
