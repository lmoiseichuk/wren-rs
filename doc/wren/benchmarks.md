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

## Against MicroPython

MicroPython 1.29.0, `ESP32_GENERIC_C6`, `_mpy` 12038, on the same board, running
`benchmarks/python/*.py` — the same four programs with the same constants.
Median of three runs each.

### Speed

| benchmark | Wren `-Os` | Wren `-O2` | MicroPython | Wren `-O2` is |
|---|---|---|---|---|
| `binary_trees` | 2.440 s | 2.160 s | 4.729 s | **2.19x faster** |
| `fib` | 3.710 s | 3.250 s | 7.109 s | **2.19x faster** |
| `list_build` | 0.150 s | 0.130 s | 0.154 s | **1.19x faster** |
| `method_call` | 0.420 s | 0.350 s | 1.748 s | **4.99x faster** |

**Wren is faster on all four**, by 1.19x to 4.99x.

The `fib` row is the one to sit with. Wren has a single numeric type, so every
value in it is a **double in software** on a part with no FPU, while
MicroPython is using small integers, which on this build are tagged pointers
and cost nothing to box. Wren is 2.19x faster *while doing the more expensive
arithmetic*. The dispatch loop is a long way ahead.

`method_call` is the widest gap (4.99x) and `list_build` the narrowest (1.19x),
which fits: the first is almost pure dispatch, where Wren wins, and the second
is almost pure memory traffic, where both are doing the same work.

### Memory

Both columns are `free` before a run minus `free` after it, **with no forced
collection on either side** — the question "what did this run consume and not
give back", asked identically. MicroPython's live set after a `gc.collect()` is
given too, because it is interesting, but it has no Wren counterpart and is not
what the middle column compares against.

| benchmark | Wren | MicroPython | MicroPython live after a collection |
|---|---|---|---|
| `binary_trees` | 78,812 B | 76,512 B — *about equal* | 50,512 B |
| `fib` | 6,612 B | 800 B — *8.3x less* | 336 B |
| `list_build` | 134,712 B | 65,440 B — *2.1x less* | 65,200 B |
| `method_call` | 15,080 B | 1,616 B — *9.3x less* | 768 B |

**MicroPython is the leaner of the two**, decisively on the two small
benchmarks and by 2x on `list_build`; `binary_trees` is a draw. This is the
opposite of what the speed table suggests and it is the more surprising result,
so it is worth being precise about where Wren's bytes go:

- **Every Wren number is a boxed double.** `fib` holds almost nothing live and
  still costs 6,612 B against 800 B.
- **A Wren list doubles its backing store**, holding the old array and the new
  one at once. `list_build`'s 134,712 B against 65,440 B is mostly that
  doubling plus 8 bytes per element where MicroPython spends 4.
- `binary_trees` is the one workload dominated by user objects rather than by
  numbers, and there the two are within 3%.

### Footprint

| | Wren `-Os` | Wren `-O2` | MicroPython |
|---|---|---|---|
| app image | **272,736 B** | 301,888 B | **1,902,128 B** |
| bootloader | 22,368 B | 22,368 B | 18,592 B |
| everything flashed | **298,176 B** | 327,328 B | **1,967,664 B** |
| free to a program | ~227,000 B | ~227,000 B | **333,344 B** |

The MicroPython image size was taken two ways that agree exactly: by walking
the running partition's ESP-IDF image header on the board, and from
`ESP32_GENERIC_C6-20260824-v1.29.0.bin` on disk. The app image is the row to
compare, since Wren's published figure is its app image too.

**Wren's image is 7.0x smaller.** That is the clearest result here, though it
is not quite like for like: this is a stock `ESP32_GENERIC_C6` build carrying
networking, TLS, `framebuf`, `btree` and the rest, against a Wren port carrying
a console. A MicroPython trimmed to match would be a great deal smaller than
1.9 MB. What the number does say is what each costs *as shipped*, which is what
somebody choosing between them actually flashes.

The free-heap row is the honest one and it goes against Wren: **MicroPython
leaves a program ~106 KB more room**. Some of Wren's shortfall is this port's
doing — the console's buffers take 67,584 B — but even discounting those
entirely it is ~295 KB against 333 KB.

The two "resident interpreter" figures are **not comparable and are left out**.
Wren's 83,036 B is malloc'd from the system heap and so is visible to the same
counter that measures programs; MicroPython's interpreter lives in static RAM
and flash, outside the GC arena that `gc.mem_free` reports, so its "992 B
allocated at boot" is not the cost of the interpreter. Comparing them would
flatter MicroPython by roughly 82 KB for no reason.

### What this adds up to

**Wren buys speed with memory.** It runs every one of these 1.2x to 5x faster
in an image a seventh the size, and pays for it with a larger per-program heap
appetite and ~106 KB less room to work in. On this part, with 512 KB of SRAM,
both are comfortable. On the 8 KB CH32V006 this project is ultimately aiming
at, neither fits as it stands — Wren spends 33 KB of stack compiling its own
core library before user code runs.

### Two notes on method

**Repeatability.** MicroPython's three runs agreed to within 0.02% on every
benchmark — 7.109166 / 7.109125 / 7.109178 for `fib`. An earlier single-run
pass recorded `binary_trees` at 6.760 s where the careful one gets 4.729 s;
that difference was the harness, not the board, and the ad-hoc wrapper was
discarded rather than averaged in.

**The heap columns were nearly wrong.** The first pass measured MicroPython
*after* a `gc.collect()` and Wren *before* one, and so reported 192 B against
134,712 B for `list_build` — a 700x result that was entirely an artefact of
asking the two sides different questions. `tools/run-micropython.py` exists to
make the measurement match `run-benchmarks.py` exactly, including a soft reset
before each run so a global left alive by the previous benchmark cannot be
charged to the next one's baseline.
