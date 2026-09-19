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
crate that a firmware pulls in the way it pulls in any other.

It is complete: **829 of 829** of upstream's own tests pass, from source and
through a bytecode round-trip, with no `unsafe` anywhere in the crate.

## Where it fits

| part | flash | RAM | verdict |
|---|---|---|---|
| CH32V003 | 16 KB | 2 KB | no |
| CH32V006 | 62 KB | 8 KB | **µwren only** — see below |
| CH32X035 | 62 KB | 20 KB | **µwren only** — see below |
| ESP32-C3 | 4 MB | 400 KB | comfortable |
| ESP32-C6 | 4 MB | 512 KB | the development board, comfortable |

**The measured floor is 22,920 B resident**, before a line of user code runs
and with the compiler left out of the image entirely. That fits an ESP32 with
room to spare and does not fit a CH32V006 at all.

### µwren

The gap on the small parts is not another feature flag. Dropping the compiler,
the transcendentals and 64-bit numbers is already possible and already counted
in that 22,920 B — what is left is **the core library itself**: the classes,
their method tables and the primitives behind them are most of the figure.

A part with 8 KB wants a deliberately reduced language — a subset of the core
library, built with the linker discarding everything unreachable — and that is
a separate deliverable from this one. It is named here rather than measured,
because nothing has been measured about it yet.

What *does* transfer to a small part today, because it was built for one:

- **Bytecode, so nothing compiles on the device.** The compiler is 34,480 B at
  `-Os` and a `.wrenc` carries a SHA-256 of the source it came from.
- **32-bit numbers**, `--features f32`: 17–20 KB smaller, 6–13% faster, and a
  fifth to a half off the heap. Not Wren — an `f32` is exact on integers only
  to 2^24 — so it is off by default.
- **A ceiling on garbage**, `Heap::set_headroom`: peak memory becomes *the live
  set plus a constant you choose*. **On a part holding few objects it is free**
  — `method_call` runs identically from a 32 KB ceiling down to 1 KB — because
  a collection costs the live set, and a small part has a small one.

## The numbers

ESP32-C6FH4 at 160 MHz, `-O3`, the four programs in `benchmarks/wren` with
identical constants on every implementation. Method and caveats:
**[`doc/wren/benchmarks-wren-rs.md`](doc/wren/benchmarks-wren-rs.md)**.

### Against C and MicroPython

| benchmark | wren-rs | C Wren `-O2` | MicroPython |
|---|---|---|---|
| `binary_trees` depth 9 | 9.104 s | **2.160 s** | 4.729 s |
| `fib(24)` ×5 | 16.966 s | **3.250 s** | 7.109 s |
| `list_build` 10,000 | 0.579 s | **0.130 s** | 0.154 s |
| `method_call` | 2.519 s | **0.350 s** | 1.748 s |
| **VM resident** | **22,920 B** | 83,036 B | — |

**Four to seven times slower than C, and 72% smaller resident.** The speed is
the honest cost of reaching objects by a bounds-checked index rather than a
pointer, which is what lets the crate forbid `unsafe`; the memory is what
compiling no core library at start-up buys.

### `f32` against `f64`

`Num` is a double in Wren, and `--features f32` makes it a single. Same board,
same commit, same programs:

| | `f64` | `f32` | |
|---|---|---|---|
| `binary_trees` | 9.104 s | **8.207 s** | −9.9% |
| `fib` | 16.966 s | **15.905 s** | −6.3% |
| `list_build` | 0.579 s | **0.525 s** | −9.3% |
| `method_call` | 2.519 s | **2.420 s** | −3.9% |
| `binary_trees` peak | 117,384 B | **94,524 B** | −19.5% |
| `list_build` peak | 132,844 B | **67,208 B** | −49.4% |
| VM resident | 22,920 B | **22,632 B** | −1.3% |
| image | 465,456 B | **447,280 B** | −3.9% |

Faster *and* smaller, because `riscv32imac` has neither the `F` nor the `D`
extension — both widths are software and the narrower one is less work. A
`Value` halves to four bytes, which is why `list_build`, whose whole memory is
a list of numbers, halves with it.

**It is not Wren, and the cost is exact rather than vague.** An `f32` is exact
on integers only to 2^24, so `list_build` — which sums to 49,995,000 — prints
**49,992,896**, and upstream's suite goes from 829 of 829 to **798**. Off by
default; conformance and every other table here are `f64`.

### Memory, from the first run on hardware

| | `binary_trees` peak | VM resident |
|---|---|---|
| first hardware run | 160,244 B | 45,676 B |
| packed method tables, `Vec` slack returned | 158,124 B | 24,680 B |
| one table per type | 133,888 B | 22,920 B |
| instance fields in chunks | **117,384 B** | 22,920 B |
| …and a 16 KB ceiling on garbage | **109,004 B** | 22,920 B |
| …and `f32` | **94,524 B** | 22,632 B |

**A third off the peak and a half off resident**, every step chosen by the heap
profiler. The two steps that were *guessed* at — reference counting and a young
generation — are the two that are switched off:
**[`doc/wren-rs/memory.md`](doc/wren-rs/memory.md)**.

### The ceiling on garbage, and why a small part pays nothing for it

`Heap::set_headroom` makes peak memory **the live set plus a constant you
choose**, instead of half as much again as the live set. Swept across its whole
range on the same board:

| ceiling | `binary_trees` (1,023 live) | its peak | `method_call` (109 live) |
|---|---|---|---|
| ratio, 1.5× *(default)* | 9.106 s | 117,376 B | 2.5185 s |
| 32 KB | 8.970 s | 121,480 B | 2.5185 s |
| 16 KB | 10.148 s | 109,004 B | 2.5185 s |
| 8 KB | 12.211 s | 109,004 B | 2.5185 s |
| 4 KB | 16.545 s | 109,000 B | 2.5185 s |
| **1 KB** | 37.916 s | **83,148 B** | **2.5185 s** |

**Read the last column.** `method_call` holds about a hundred objects and does
not move — not to five decimal places — from the loosest setting to the
tightest. `binary_trees` holds a thousand-node tree and pays four times over
for the same ceiling.

That is not a coincidence, it is what tracing costs: **a collection is
proportional to the live set.** So the parts that most need a tight bound — a
CH32V006 with 8 KB, where a program holds tens of objects rather than thousands
— are precisely the ones where tightening it is free. The default stays the
ratio so the published numbers remain comparable with the C port; a firmware
that knows its live set should not keep it.

Two more things the curve says. Between 16 KB and 4 KB the peak barely moves
while the time doubles — below 16 KB it is no longer floating garbage that sets
the peak but the live set, the field chunks and the tables' high-water mark, so
there is nothing left to squeeze. And 32 KB is *looser* than 1.5× of this live
set, which is why it is both slightly faster and slightly larger.

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

### What the measurements changed

Four findings, each of which overturned something this repository had written
down. They are the reason the two documents below are worth reading rather than
skimming.

**The design note's central estimate was wrong by an order of magnitude.** It
put the cost of reaching objects by a bounds-checked index rather than a
pointer at "single-digit to low-double-digit percent". The first run on
hardware said **four to eight times**. The note had reasoned about one bounds
check per field access; a single method call traversed *ten* heap lookups, and
six of them were asking for the same two objects over and over.
→ [`design.md`](doc/wren-rs/design.md)

**The host could not see any of the speed work.** Every dispatch change
measured inside the noise on a workstation and 10–15% on the board. An
out-of-order core hides a dependent load that an in-order RISC-V pays for in
full — so a laptop cannot tell you whether an indirection matters.
→ [`design.md`](doc/wren-rs/design.md)

**All the garbage is acyclic, and it did not help.** The heap profiler found
that across 873 programs, **100%** of what dies could be reclaimed by a
reference count, and **84%** of allocations die before the next collection.
Both findings are real. Both suggested replacing mark-sweep. All three
candidates were built, machine-verified and measured — and **none of them
stayed**, because collection is only 16.9% of the one benchmark that allocates
and every replacement costs work proportional to what the program *does*.
→ [`memory.md`](doc/wren-rs/memory.md)

**The biggest thing on the heap was not the objects.** It was the method
tables, then the allocator's own per-object headers. Neither had been suspected;
both were found by counting. → [`memory.md`](doc/wren-rs/memory.md)

The full tables — every benchmark at both optimisation levels, against C Wren
and MicroPython, with the method and caveats — are in
**[`doc/wren/benchmarks-wren-rs.md`](doc/wren/benchmarks-wren-rs.md)**.

### Two documents

**[`doc/wren-rs/design.md`](doc/wren-rs/design.md)** — the decisions and what
they cost. NaN tagging in safe Rust; why an object carries **no header at all**
where upstream spends sixteen bytes; the measured size of every object type on
a 32-bit part; what a 32-bit `Num` would buy and what it breaks.

**[`doc/wren-rs/memory.md`](doc/wren-rs/memory.md)** — where memory goes and
what reclaims it. The heap census, which found the biggest item twice and was
twice a surprise; instance fields in adaptive chunks; three replacements for
mark-sweep built, verified and rejected, each with the number that justified it
and how that number was misread; and the ceiling on garbage that **costs
nothing at all on a part holding few objects**, which is the setting a CH32
wants.

### Step 4 measured: shipping bytecode

`.wren` is compiled on a workstation to `.wrenc` and the device is handed that,
so the lexer and the parser are never linked. The format carries a SHA-256 of
the source it came from, which is what makes bytecode-in-the-tree checkable
rather than a blob nobody can trace:

```sh
tools/build-bytecode.sh           # rebuild every .wrenc
tools/build-bytecode.sh --check   # verify each against its source
```

**The suite passes 829 of 829 through the round-trip as well** — every test
compiled, written, re-loaded into a fresh VM and run. That is the claim that
matters before any number: the two paths produce the same program.

**What the compiler is worth**, from one package built twice with and without
`wren/compiler`:

| | `-Os` | `-O3` |
|---|---|---|
| `f64` bytecode / with compiler | 244,960 B / 279,440 B | 363,568 B / 444,784 B |
| `f32` bytecode / with compiler | 227,712 B / 262,256 B | 343,776 B / 425,104 B |
| **the compiler** | **~34,500 B** | **~81,200 B** |

It barely moves with the number width — 34,480 B against 34,544 — because a
lexer and a parser do not care how wide a double is.

Loading a program takes 756–1,924 µs where compiling it takes 25–35 ms. That is
6.5× and it barely matters against a program that then runs for seconds — and
it is the dominant number on the duty cycle these parts are bought for, where a
node wakes, samples and sleeps.

**What bytecode costs is everything a compiler would have allowed**: no REPL,
no `eval`, no program arriving over the air as text.

`ports/esp32c6-wren-boot` is the shape a device would actually take — it looks
for `boot` and then `main`, MicroPython's convention, and is **one package
built twice** so the only difference between the two images is that feature.
Details and the programs themselves:
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
