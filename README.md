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
programs/       boot.wren and main.wren, and the bytecode built from them
doc/wren/       what upstream Wren and MicroPython measured
doc/wren-rs/    why the Rust implementation is built the way it is
ports/          one directory per (board, implementation) pair
  esp32c6-wren/          step 1, the C reference
  esp32c6-micropython/   step 2, the baseline
  esp32c6-wren-rs/       step 3, the deliverable
  esp32c6-wrenc-rs/      step 4, the same benchmarks with no compiler linked
  esp32c6-wren-boot/     step 4, a node that looks for boot and main
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
| **step 3 — the Rust VM** | **the language is complete** |
| — upstream's test suite | **829 of 829** |
| — conformance probes | **96 of 96** |
| — this crate's own tests | 193 |
| **step 4 — bytecode** | **done, measured** |
| — the suite, run from `.wrenc` | **829 of 829** |
| — the suite in an `f32` build | 798 of 829 — *not Wren, and off by default* |

### Step 3: the language is complete

`crates/wren` passes **all 829** of upstream Wren's own tests — every group,
including `core`, `language`, `limit`, `meta`, `random` and `regression` — run
unmodified and scored against the `// expect:` comments they already carry.
That is the same contract the C port was held to in step 1, where upstream
itself scored 821 of 846 on an ESP32-C6.

Everything the language has: classes with constructors, fields, inheritance,
`super`, static members and static fields; closures with upvalues; fibers as
real coroutines, and the error handling built on them; modules and `import`;
maps as a hash table; `Sequence` with lazy `map`/`where`/`take`/`skip`; class
attributes; string interpolation and Wren's UTF-8 rules. No `unsafe`.

**One difference that matters for benchmarking, stated before any numbers.**
Upstream writes a good deal of its core library *in Wren* — `Sequence` and its
methods live in `wren_core.wren` and are compiled at every start-up. Here they
are Rust primitives. That is a deliberate choice for the 8 KB target, where
compiling several hundred lines of core library before user code runs is
unaffordable, and it is why this VM starts faster and uses less stack than
upstream's. It also means **any benchmark leaning on `Sequence` measures that
choice rather than the interpreter**, and would flatter this implementation for
a reason that has nothing to do with the VM. The four benchmark programs do not
use those methods — they are loops, field access, arithmetic and dispatch — so
the published comparison is unaffected; a different benchmark might not be.

### Step 3 measured

All three implementations on the same ESP32-C6FH4 @ 160 MHz, running the same
four programs from `benchmarks/wren` with identical constants. Method and
caveats: **[`doc/wren/benchmarks-wren-rs.md`](doc/wren/benchmarks-wren-rs.md)**.

**Speed** — each program's own `System.clock` figure, in seconds:

| benchmark | wren-rs `speed` | wren-rs `size` | C Wren `-O2` | C Wren `-Os` | MicroPython |
|---|---|---|---|---|---|
| `binary_trees` depth 9 | 8.455 | 15.371 | **2.160** | 2.440 | 4.729 |
| `fib(24)` x5 | 17.253 | 33.669 | **3.250** | 3.710 | 7.109 |
| `list_build` 10,000 | 0.577 | 1.028 | **0.130** | 0.150 | 0.154 |
| `method_call` | 2.411 | 4.326 | **0.350** | 0.420 | 1.748 |

The first run on hardware was slower than this — `binary_trees` 8.616,
`fib` 18.176, `list_build` 0.574, `method_call` 2.766 at `speed`. What moved
them is in **[what optimisation was worth](#what-optimisation-was-worth)**
below.

**Memory** — VM resident before any user code, and heap consumed per program
(free before minus free after, no forced collection, measured the same way on
both):

| | wren-rs | C Wren `-O2` | MicroPython |
|---|---|---|---|
| **VM resident** | **22,920 B** | 83,036 B | — |
| free to a program | ~305,000 B | ~227,000 B | 333,344 B |
| `binary_trees` | 133,880 B | 78,812 B | 76,512 B |
| `fib` | **4,172 B** | 6,612 B | 800 B |
| `list_build` | 132,460 B | 134,712 B | 65,440 B |
| `method_call` | **8,316 B** | 15,080 B | 1,616 B |

**Footprint** — and these do *not* compare across implementations, because the
platforms differ: the C port is an ESP-IDF application carrying FreeRTOS and
newlib, wren-rs is bare metal carrying neither, and MicroPython is a stock
build with networking and TLS in it.

| | `size` | `speed` |
|---|---|---|
| wren-rs full image | 278,144 B | 440,576 B |
| the same firmware with no VM | 111,728 B | 113,472 B |
| **wren-rs VM contribution** | **166,416 B** | **327,104 B** |
| C Wren full ESP-IDF image | 272,736 B | 301,888 B |
| MicroPython full image | 1,902,128 B | — |

#### What these say

**The VM is 72% smaller resident and three to seven times slower.** Both halves
are the same design, and only one of them was predicted.

The memory result is what compiling no core library at start-up buys: upstream
builds `wren_core.wren` every time a VM is created, and this does not. 22,920 B
against 83,036 B is the figure that decides whether a part is usable at all.

The speed result contradicts this repository's own design note, which put the
cost of reaching objects by index rather than by pointer at "single-digit to
low-double-digit percent". It was 4–8x on the first run and is 4–7x after the
work described below, worst on `method_call`, which is almost pure dispatch.
[`doc/wren-rs/design.md`](doc/wren-rs/design.md) is corrected and says why the
estimate was wrong: it was reasoned about as one bounds check per
field access, and a single method call traverses six references — receiver to
class, class through its box, class to method table, method to closure, closure
to function, function to chunk. Upstream follows a pointer at each.

`binary_trees` is the one memory row that loses, and for a known reason: every
object here occupies 24 bytes before its contents, the size of the largest
variant of one enum, where upstream allocates each type at its own size. A tree
of instances is exactly the workload that pays for it.

#### What optimisation was worth

The first run on hardware was published untuned, because a result that
contradicts the design is worth more than a flattering one. Acting on it since:

| | first run | now |
|---|---|---|
| VM resident | 45,676 B | **22,920 B** |
| `method_call` | 2.766 s | **2.411 s** |
| `fib` | 18.176 s | **17.253 s** |
| `binary_trees` | 8.616 s | **8.455 s** |
| `binary_trees` heap | 160,244 B | **133,880 B** |
| `method_call` heap | 9,800 B | **8,316 B** |

Three changes, each measured on the board:

**The six indirections were partly redundant.** Entering a call asked the heap
for the same two objects three times over — once for the arity, once for the
code, once for the module — and returning did the walk again for the caller's
chunk. Field access fetched its offset through closure and function on every
read and every write. Method lookup walked the superclass chain where upstream
copies a parent's methods down at class creation. Four lookups per call now,
where there were ten.

**The method tables were the largest thing on the heap**, not the instances: a
class's table is indexed by global method symbol, so it is as long as the
highest symbol that class answers to and almost all of it is empty. Handing back
the `Vec` slack and packing an entry from 8 bytes to 4 took VM resident from
49,004 B to 24,680 B.

**Every object was charged for the largest one.** A slot held an `Object`, and
an enum is as large as its largest variant, so a `List` that needs twelve bytes
was charged twenty-four. One table per type charges each what it costs, and it
made the benchmarks marginally *faster* as well: the type now lives in the
handle, so seven of `class_of`'s ten answers touch no memory at all.

**The host measured none of the speed work.** Every one of those changes was
inside the noise on a workstation, because an out-of-order core hides a
dependent load that an in-order RISC-V pays for in full. The board is the only
place a change like this can be judged.

#### Three replacements for the collector, all measured, none kept

The heap profiler said the garbage 873 programs produce is 100% acyclic and
that 84% of allocations die before the next collection. Both findings are
real, and both suggested replacing mark-sweep. So all three candidates were
built, verified against the whole suite, and measured on the board —
`binary_trees`, speed profile:

| | time | peak |
|---|---|---|
| **tracing, as it stands** | **8.455 s** | 133,880 B |
| tracing + a 16 KB garbage ceiling | 9.240 s | **121,624 B** |
| deferred reference counting | 9.630 s | 126,056 B |
| a young generation + that ceiling | 9.105 s | 176,736 B |

**Nothing beats the collector already there on time, and the simplest change
beats everything on memory.** The profile had said why in advance: collection
is 16.9% of the one benchmark that allocates and 0.0–0.6% of the other three,
so a replacement can win at most 17% of one program — while each of these adds
work proportional to what the program *does* rather than to what the collector
*costs*.

Refcounting is behind `--features refcount` and a nursery behind
`--features nursery`; both are correct — 829 of 829, with their write barriers
machine-verified — and both are off. `Heap::set_headroom` is the one that
stayed useful: it makes peak memory *the live set plus a constant* instead of
half as much again as the live set, which is the shape a fixed heap wants.

[`doc/wren-rs/design.md`](doc/wren-rs/design.md) carries the measurements and
what each cost.
[`doc/wren-rs/design.md`](doc/wren-rs/design.md) carries the measurements and
what each would cost.

### Step 4 measured: shipping bytecode

`.wren` is compiled on a workstation to `.wrenc` and the device is handed that,
so the lexer and the parser are never linked. The format carries a SHA-256 of
the source it came from, which is what makes bytecode-in-the-tree checkable
rather than a blob nobody can trace:

```sh
tools/build-bytecode.sh           # rebuild every .wrenc
tools/build-bytecode.sh --check   # verify each against its source
```

**The suite passes 829 of 829 through the bytecode round-trip as well** — every
test compiled, written, re-loaded into a fresh VM and run. That is the claim
that matters before any number below: the two paths produce the same program.

**Four builds, no compiler linked** — the configuration a device actually
ships. Same board, same bytecode; the only differences are the optimiser and
whether a number is a double or a single.

| | `-Os` f64 | `-Os` f32 | `-O3` f64 | `-O3` f32 |
|---|---|---|---|---|
| `binary_trees` depth 9 | 14.172 s | 13.042 s | 7.882 s | **7.015 s** |
| `fib(24)` x5 | 32.642 s | 29.739 s | 16.737 s | **14.638 s** |
| `list_build` 10,000 | 0.992 s | 0.891 s | 0.570 s | **0.495 s** |
| `method_call` | 4.183 s | 3.914 s | 2.382 s | **2.128 s** |
| image | 244,416 B | **227,184 B** | 360,816 B | 341,056 B |
| VM resident | 24,680 B | **23,880 B** | 24,680 B | **23,880 B** |
| `binary_trees` heap | 155,676 B | **120,772 B** | 155,672 B | **120,772 B** |
| `list_build` heap | 132,096 B | **66,508 B** | 132,096 B | **66,508 B** |
| loading a program | 785–1,924 µs | 781–1,924 µs | 758–1,375 µs | 756–1,416 µs |

**`f32` is 6–13% faster, 17–20 KB smaller, and takes a fifth to a half off the
heap.** The part has neither the `F` nor the `D` extension, so both widths are
software and the narrower one is simply less work; `list_build`'s heap halves
because a list of 10,000 numbers is 10,000 `Value`s.

**It is not Wren, and the cost is exact rather than vague.** An `f32` holds
integers exactly only to 2^24, so `list_build` — which sums to 49,995,000 —
prints **49,992,896**. Upstream's suite goes from 829 of 829 to 798, every
failure a precision one. The feature is off by default and conformance runs and
every other table here are `f64`.

**The compiler is a flat cost on top of any of these**, which is why it is out
of the table rather than in it. One package built twice, with and without
`wren/compiler`:

| | `-Os` | `-O3` |
|---|---|---|
| `f64` bytecode / with compiler | 244,960 B / 279,440 B | 363,568 B / 444,784 B |
| `f32` bytecode / with compiler | 227,712 B / 262,256 B | 343,776 B / 425,104 B |
| **the compiler** | **~34,500 B** | **~81,200 B** |

It barely moves with the number width — 34,480 B against 34,544 B — because a
lexer and a parser do not care how wide a double is.

#### A node, not a harness

`ports/esp32c6-wren-boot` is the shape a device would actually take: at
start-up it looks for a program called `boot`, runs it, then looks for `main`
and runs that — MicroPython's convention, and a missing name is skipped rather
than faulted. It is **one package built twice**, so the only difference between
the two images is whether `wren/compiler` is on.

| `-Os` | bytecode | compiler |
|---|---|---|
| image | **244,960 B** | 279,440 B |
| prepare, both files | **9,821 µs** | 64,154 µs |
| run, both files | 3,196,086 µs | 3,211,906 µs |
| reset to idle | **3,228,083 µs** | 3,298,642 µs |
| heap left | **183,040 B** | 172,552 B |

**Preparation is 6.5x faster and here it barely matters** — 9.8 ms against
64.2 ms, against programs that then run for three seconds. It becomes the
dominant number on the duty cycle these parts are actually bought for: a node
that wakes, samples and sleeps pays prepare on every wake and the run cost for
a few milliseconds.

**Running is a wash, as it should be.** Both builds execute the same bytecode
through the same interpreter, and that the two agree is the evidence the
comparison is sound.

**What bytecode costs is everything a compiler would have allowed.** No REPL,
no `eval`, no accepting a program that arrives over the air as text. That is
the trade, and ~34,500 B is what it is worth.

Details, and the programs themselves:
**[`ports/esp32c6-wren-boot/README.md`](ports/esp32c6-wren-boot/README.md)**.

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
