# esp32c6-micropython — the baseline

MicroPython on the same ESP32-C6, measured the same way.

**This is the number worth beating**, because it is what somebody would
otherwise reach for. A Rust VM that is smaller and faster than a C VM is
interesting; one that is smaller and faster than the thing people actually
deploy is useful.

It was equally possible that MicroPython would win on some axis, and it does —
see the results below. That is a finding and it goes in the table like any
other. A comparison arranged so that only one answer can come out is not a
comparison.

## Status

**Done.** MicroPython 1.29.0, `ESP32_GENERIC_C6`, built 2026-08-24, `_mpy`
12038 — the released
`ESP32_GENERIC_C6-20260824-v1.29.0.bin`, not vendored here because it is a
third-party 1.9 MB binary that micropython.org already serves. Results are in **[`doc/wren/benchmarks.md`](../../doc/wren/benchmarks.md)**
— that is the only place the numbers live.

The short version: Wren is **1.19x to 4.99x faster** on all four benchmarks and
its image is **7.0x smaller**, while MicroPython leaves a program **~106 KB more
heap** and uses **2x to 9x less heap per benchmark**.

## What it needs

Less than the others, because nothing is built here:

* **A released MicroPython build** for `ESP32_GENERIC_C6`, from
  [micropython.org/download](https://micropython.org/download/ESP32_GENERIC_C6/).
* **`esptool`** to flash it — `pip install esptool`, or the one ESP-IDF already
  provides if it is installed.
* **`python3-serial`** for the runners in `tools/`.

```sh
esptool.py --chip esp32c6 -p <by-id path> erase_flash
esptool.py --chip esp32c6 -p <by-id path> --baud 460800 \
           write_flash -z 0x0 ESP32_GENERIC_C6-<version>.bin
```

**Use a released build, not one compiled here.** The point of this port is to
measure what somebody would actually deploy; a locally built MicroPython with
different options would be measuring our build choices rather than theirs. It is
also why the image-size comparison is noted as unfair to MicroPython in the
report — a stock build carries networking, TLS, `framebuf` and `btree` that a
Wren port carrying a console does not.

## Running it

```sh
tools/run-micropython.py <by-id path> 3      # three runs of each benchmark
```

The benchmarks are `benchmarks/python/*.py`, beside their Wren counterparts in
`benchmarks/wren/` with identical constants. **They are not kept here.** An
earlier copy in this directory drifted from the Wren originals and from the
runner, and produced two numbers that had to be withdrawn.

Two things the runner does deliberately, both learned the hard way:

**Raw REPL, not paste mode.** Paste mode echoes every line, which fills the
board's transmit buffer, blocks its reader and truncates the file — the same
failure the Wren console hit from the other direction.

**A soft reset before every run, and no forced collection inside the measured
window.** `list_build` leaves a 10,000-element list alive at module scope, which
would otherwise be charged against the next benchmark's baseline; and a
`gc.collect()` before the final reading would answer a different question from
the one the Wren port answers. Getting this wrong the first time reported 192 B
against Wren's 134,712 B for the same program.

## How this differs from Wren

Python is not Wren, and the benchmark translation is the weak joint in the whole
comparison. **Each divergence is noted in the benchmark file at the point it
occurs** — `benchmarks/python/*.py` — rather than restated here, where it would
be free to drift from the code it describes.

The one worth knowing before reading any number: **Wren has a single numeric
type**, so every value in `fib` is a double, and on a part with no hardware
floating point that is soft-float. MicroPython uses small integers, which are
far cheaper. Wren wins that benchmark anyway, which makes the result stronger
rather than weaker — but the gap is a *representation* difference as much as an
interpreter one.
