# Wren on the ESP32-C6 — measured

Generated from `benchmarks-size.json` and `benchmarks-perf.json`.

ESP32-C6FH4 rev v0.2 @ 160 MHz, ESP-IDF v5.5, upstream Wren 0.4.0 unmodified.
Median of three runs. The programs are in `benchmarks/wren`, scaled to fit this
part — see `benchmarks/README.md` for what changed and why.

## Speed

| benchmark | `-Os` | `-O2` | gain | what it exercises |
|---|---|---|---|---|
| `binary_trees` | 2.440 s | 2.160 s | **+13.0%** | allocation and the collector |
| `fib` | 3.710 s | 3.250 s | **+14.2%** | method dispatch and arithmetic |
| `list_build` | 0.150 s | 0.130 s | **+15.4%** | list growth and iteration |
| `method_call` | 0.420 s | 0.350 s | **+20.0%** | dispatch, including `super` |

**`-O2` buys 13–20% for 29 KB of flash** — a 10.7% larger image (272,736 B →
301,888 B). Whether that is worth it depends on which budget is tighter, which
is the whole reason both are published rather than one.

What no optimisation level changes: the C6 is RV32IMAC with **no hardware
floating point**, and Wren has a single numeric type, so every number is a
double in software. That cost is in every row above.

## Memory

Identical between the two builds, as expected — optimisation changes code, not
data structures.

| benchmark | heap | stack |
|---|---|---|
| `binary_trees` | 78,812 B | 33,552 B |
| `fib` | 6,612 B | 33,552 B |
| `list_build` | 134,712 B | 33,552 B |
| `method_call` | 15,080 B | 33,552 B |

## The VM itself

| | |
|---|---|
| image, `-Os` | **272,736 B** |
| image, `-O2` | 301,888 B |
| heap free at boot | 310,476 B |
| — of which the console's buffers | 67,584 B *(harness, not Wren)* |
| **VM resident after `wrenNewVM`** | **83,036 B** |
| **compiler stack high-water** | **33,552 B** |
| heap left for a program | ~227,000 B |

The stack figure is the one to sit with: **33 KB is spent inside `wrenNewVM`**,
compiling Wren's own core library, before a line of user code runs. A CH32V006
has 8 KB of RAM in total.

## What would not fit

Upstream's own benchmarks, unmodified, on this part:

| benchmark | asks for | outcome |
|---|---|---|
| `for` | a 1,000,000-element list — 8 MB of values | null store, reboot |
| `binary_trees` | a depth-13 stretch tree — ~768 KB | null store, reboot |
| `map_numeric`, `fibers` | likewise | reboot |

None is a Wren defect. They are programs larger than the machine. The ceiling
was pinned by bisection rather than estimated: `binary_trees` at depth 10
completes its depth-11 stretch tree and the depth 4, 6 and 8 iterations, then
dies building depth-10 trees alongside the live long-lived tree — so roughly
**200 KB of simultaneous live objects** is the limit here.

A related one worth knowing: **a Wren list doubles its backing store on
growth**, holding the old array and the new one at once. 20,000 elements is
~160 KB steady but ~240 KB across the doubling, which is why `list_build` uses
10,000.

## Still to come

MicroPython 1.29 on the same board, running `benchmarks/python/*.py` — the same
programs with the same constants. Until those land, nothing here is a
comparison; it is one implementation measured.

