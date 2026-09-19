# wren-rs

Re-implementing [Wren](https://wren.io/) in Rust, small enough to embed in a
microcontroller — and measured against MicroPython on the same hardware so the
claim is a number rather than an opinion.

Wren is a good candidate for this. It is a class-based scripting language with
a compact bytecode VM, closures, fibers and a real garbage collector, and the
reference implementation in C is about six thousand lines. Small, but not
trivial: it is the smallest interesting target rather than a toy.

## What this is for

**A pure-Rust `cargo add` VM for small RISC-V and Xtensa parts.** No C build
step, no submodule, no `build.rs` compiling somebody else's tree — a `no_std`
crate that a firmware pulls in the way it pulls in any other. That is the
deliverable; everything below is how it gets there and how it gets checked.

The parts in view, smallest first, because the smallest is what decides the
design:

| part | flash | RAM | verdict |
|---|---|---|---|
| CH32V006 | 62 KB | 8 KB | the hard limit — an interpreter here is a stretch and may not land |
| CH32X035 | 62 KB | 20 KB | plausible with a tuned heap |
| ESP32-C3 | 4 MB | 400 KB | comfortable |
| ESP32-C6 | 4 MB | 512 KB | the development board, comfortable |

8 KB of RAM is the number that shapes everything. A Wren `Obj` header, a value
representation, a call frame and a GC all have to fit inside it with a user
program still running, and the honest answer may be that the smallest parts get
a subset rather than the language. Deciding that from measurement rather than
from optimism is part of the work.

## The plan

Six steps, each producing something checkable.

**1 — the reference on the board.** Upstream Wren, unmodified, on the ESP32-C6,
flashable and talking over a TTY. This is not the deliverable; it is the
*control*.

What it controls for is worth stating plainly, because it is the actual question
this project asks: **the delta between C written by people over years and Rust
written by a language model in an afternoon.** Wren's C is careful, tuned and
mature. If the Rust comes out larger and slower, that is the honest result and
the interesting one — it says where the gap is. If it comes out competitive, that
is only meaningful because the same benchmark ran on the same board against the
same reference.

Upstream is a git submodule at `vendor/wren`, pinned and **never modified**.

**2 — MicroPython as the baseline.** Flash it on the same board and measure
start-up time, a benchmark set, and steady-state RAM. MicroPython is the thing
people actually reach for on these parts, so it is the number worth beating —
or worth honestly losing to, in which case that is the finding.

**3 — the Rust VM.** Re-implement: lexer, compiler, bytecode VM, GC. `no_std`,
allocator-pluggable, feature-gated so a part with 8 KB can leave out what it
cannot afford. Host-tested throughout; the board is where it is confirmed, not
where it is developed.

**4 — bytecode.** Compile ahead of time on a host and ship the bytecode, so a
constrained part never carries the compiler. This is what makes CH32-class
targets possible at all.

**5 — measure.** Size, speed and memory against both step 1 and step 2, on real
hardware. Publish the deltas.

**6 — speculative: a hot-spot VM.** Translate hot bytecode chunks to native
RISC-V. Interesting, unproven, and explicitly last — it only makes sense once
there is something whose hot paths are worth finding.

## Why the order

Steps 1 and 2 produce no Rust and are tempting to skip. They are first because
**a performance claim without a baseline is not a claim.** "Faster than
MicroPython" means nothing until MicroPython has been run on this board, with
these benchmarks, and written down.

Step 4 before step 6 for the same reason: shipping bytecode is a large, certain
win in flash and RAM, and a JIT is a speculative win in speed. Do the certain
one first.

## Layout

```
crates/wren/    the Rust VM -- the crate a firmware depends on
vendor/wren/    upstream wren-lang/wren, submodule, unmodified
benchmarks/     the same programs in both languages, same constants
doc/wren/       what upstream Wren and MicroPython measured
doc/wren-rs/    why the Rust implementation is built the way it is
ports/          one directory per (board, implementation) pair
  esp32c6-wren/          step 1, the C reference
  esp32c6-micropython/   step 2, the baseline
  esp32c6-wren-rs/       step 3, the deliverable
```

**`crates/wren` is a standalone package**, and the workspace root carries no
code of its own. Somebody adding this to a firmware takes that crate and none of
the rest: the vendored upstream, the harnesses and the ports all exist to
measure it, not to ship with it.

`ports/README.md` holds the rule that makes the three comparable, and what each
one has to provide.

## Status

| | |
|---|---|
| upstream submodule | pinned at 0.4.0 |
| **step 1 — C reference on the C6** | **done, measured** |
| **step 2 — MicroPython baseline** | **done, measured** |
| **step 3 — the Rust VM** | **started** |
| — lexer | written, 24 tests |
| — values, objects, collector | written, 28 tests |
| — compiler, interpreter loop | not started |

### Steps 1 and 2: the numbers to beat

Upstream Wren 0.4.0 runs on an ESP32-C6 and passes **821 of 846** of its own
language tests there (97.0%). It was then measured against MicroPython 1.29.0
on the same board, running the same four programs with the same constants.
Full method and caveats in **[`doc/wren/benchmarks.md`](doc/wren/benchmarks.md)**;
the suite results are in [`doc/wren/README.md`](doc/wren/README.md).

| | Wren `-O2` | MicroPython | |
|---|---|---|---|
| `fib(24)` x5 | **3.250 s** | 7.109 s | Wren 2.19x faster |
| `binary_trees` depth 9 | **2.160 s** | 4.729 s | Wren 2.19x faster |
| `method_call` | **0.350 s** | 1.748 s | Wren 4.99x faster |
| `list_build` 10,000 | **0.130 s** | 0.154 s | Wren 1.19x faster |
| app image (`-Os`) | **272,736 B** | 1,902,128 B | Wren 7.0x smaller |
| free to a program | 227,000 B | **333,344 B** | MicroPython 1.5x roomier |
| heap for `list_build` | 134,712 B | **65,440 B** | MicroPython 2.1x leaner |

**Wren buys speed with memory**: faster on all four, in a seventh of the flash,
and paying for it in heap. The image comparison is against a stock
`ESP32_GENERIC_C6` carrying networking and TLS, so it flatters Wren; the heap
and speed figures are like for like.

### Step 3 so far

The lexer, the value representation, the object model and a mark-sweep collector
over it — 52 tests, `#![forbid(unsafe_code)]`, and it builds for
`riscv32imac-unknown-none-elf` with and without an allocator.

**The central decision is the object representation**, because most of the rest
follows from it. Values are NaN-tagged into 8 bytes as upstream does, but
objects are reached by a 4-byte handle into one table rather than by pointer —
which means an object carries **no header at all** where upstream spends 16
bytes on one, the collector can be replaced without touching the rest of the VM,
and none of it needs `unsafe`. What it costs is set out beside what it buys in
[`crates/wren/README.md`](crates/wren/README.md), with the full argument and the
measured layout in [`doc/wren-rs/design.md`](doc/wren-rs/design.md).

### What step 1 established about the small parts

Four findings the Rust implementation inherits as requirements — see
[`ports/esp32c6-wren`](ports/esp32c6-wren) for the detail.

| | |
|---|---|
| app image, `-Os` | 272,736 B |
| **VM resident** | **83,036 B** |
| **compiler stack** | **33,552 B** |
| live-object ceiling on this part | ~200 KB |

**Two of them bear on whether the small parts are reachable at all.** The
compiler needs 33 KB of stack, which is four times a CH32V006's entire RAM — the
strongest argument yet for step 4, where bytecode is built on a host so the part
never carries a compiler. And upstream's default garbage-collector thresholds
(10 MB before the first collection) mean that **out of the box it crashes on
this class of part**, with a null store rather than a diagnostic.

The lexer came first because it needs no hardware and is where the host tests
start. Everything else waits on a board.

## Licence

MIT.
