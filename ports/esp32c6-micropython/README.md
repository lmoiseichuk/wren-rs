# esp32c6-micropython — the baseline

MicroPython on the same ESP32-C6, measured the same way.

**This is the number worth beating**, because it is what somebody would
otherwise reach for. A Rust VM that is smaller and faster than a C VM is
interesting; one that is smaller and faster than the thing people actually
deploy is useful.

It is equally possible that MicroPython wins on some axis — start-up, or breadth
of library, or sheer maturity. That is a finding and it goes in the table like
any other. A comparison arranged so that only one answer can come out is not a
comparison.

## What it needs

Less than the others, because nothing is built here:

* **A released MicroPython build** for `ESP32_GENERIC_C6`, from
  [micropython.org/download](https://micropython.org/download/ESP32_GENERIC_C6/).
* **`esptool`** to flash it — `pip install esptool`, or the one ESP-IDF already
  provides if it is installed.
* **`python3-serial`** for `tools/run-python.py`.

```sh
esptool.py --chip esp32c6 -p <by-id path> erase_flash
esptool.py --chip esp32c6 -p <by-id path> --baud 460800 \
           write_flash -z 0x0 ESP32_GENERIC_C6-<version>.bin
```

**Use a released build, not one compiled here.** The point of this port is to
measure what somebody would actually deploy; a locally built MicroPython with
different options would be measuring our build choices rather than theirs.

## Status

Waiting on a flash. This one is a flashing exercise rather than a port to write:
a released MicroPython build for `ESP32_GENERIC_C6`, from micropython.org.

```
# flash whatever release is current, then:
tools/run-python.py <by-id path> ports/esp32c6-micropython/bench.py
```

`bench.py` carries the three benchmarks translated from Wren, and
`tools/run-python.py` sends it over the **raw** REPL rather than paste mode —
paste mode echoes every line, which fills the board's transmit buffer, blocks
its reader and truncates the file. That is the same failure the Wren console hit
from the other direction, and it cost a debugging round there.

## What to measure

* **Image size** on flash, and static RAM at rest.
* **Free heap after start-up** — the memory a user program actually gets.
* **Start-up time** to a usable prompt.
* **The shared benchmark set**, translated to Python with as little cleverness as
  possible, because a benchmark tuned for one language and transliterated into
  another measures the translation.

## How this differs

Python is not Wren, and the benchmark translation is the weak joint in the whole
comparison. `bench.py` notes each divergence at the point it occurs; two are
worth knowing before reading any number:

**`loop` is not doing the same arithmetic.** Wren has a single numeric type and
every value in that benchmark is a double — so on a part with no hardware
floating point it is soft-float. MicroPython uses small integers, which are far
cheaper. The comparison is still worth making, because it is what each language
actually does with the same program, but the gap there is a *representation*
difference and reading it as an interpreter difference would be wrong.

**`fib` uses a classmethod and `tree` avoids `__slots__`**, both deliberately.
A module-level function and slotted objects would be measurably faster and would
stop measuring the thing Wren is being compared on — method dispatch, and
instances with fields.
