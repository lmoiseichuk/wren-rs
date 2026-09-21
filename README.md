# wren-rs

Technically that is a Friday project which goes out of control for whole weekend.
As I have several projects on ch32xxx and esp32cX I am looking something to
replace MicroPython but a bit more modern then forth, lisp, basic or lua.

The Squirrel/Quirrel was strong candidate but suddently [wren](https://github.com/wren-lang/wren)
hit my attention.  Wren is a good candidate for this. It is a class-based 
scripting language with a compact bytecode VM, closures, fibers and a real 
garbage collector, and the reference implementation in C is about six thousand lines. 
Small, but not trivial: it is the smallest interesting target rather than a toy.


With Claude help porting to esp32c6 (as it was plugged)
happened fast but memory consumption demonstrated higher then for MicroPython.

And here everything goes under control, so now what is added:
- rust version of wren
- boot.wren / main.wren during start as Micropython does
- nofp/fp32/fp64 support, on slow emulating platforms fp64 is no-go
- bytecode generator
- [b]uwren[/b] - packing image (bare metal / nostd) for your bytecode
- memory shrinked to ch32v006 8KB and 62KB flash (see below)
- primitive but working allocator with very minimal overhead
- ... something else, I do not remember

Basically very early draft, still slower then original but not much. 

## What this is for

**A pure-Rust `cargo add` VM for small RISC-V and Xtensa parts.** No C build
step, no submodule, no `build.rs` compiling somebody else's tree — a `no_std`
crate that a firmware pulls in the way it pulls in any other.

It is complete: **829 of 829** of upstream's own tests pass, from source and
through a bytecode round-trip. The object model, the collector and the
compiler contain no `unsafe`; the interpreter's instruction fetch does, and
says why at each site.

## Where it fits

| part | flash | RAM | verdict |
|---|---|---|---|
| CH32V003 | 16 KB | 2 KB | no |
| CH32V006 | 62 KB | 8 KB | **µwren only** — see below |
| CH32X035 | 62 KB | 20 KB | **µwren only** — see below |
| ESP32-C3 | 4 MB | 400 KB | comfortable |
| ESP32-C6 | 4 MB | 512 KB | the development board, comfortable |

**The measured floor is 23,176 B resident** for full Wren, before a line of
user code runs and with the compiler left out of the image entirely; 21,316 B
with `nofp`. That fits an ESP32 with room to spare and does not fit a CH32V006.

**Tailored to one program it is 4,008 B.** A `.wrenc` file names every method
its code can call and every variable it can reach, so a VM can be built with
only those -- and a build step can freeze the resulting classes, method tables
and names into the image rather than constructing them in RAM at start-up. On
`fib` that leaves nothing of the core in the heap at all, and a run peaking at
8,528 B against a CH32V006's 8,192 -- and 54,696 B of `wren` in flash against
its 63,488. See
**[`doc/wren_native.md`](doc/wren_native.md)** and
**[`doc/wren-rs/memory.md`](doc/wren-rs/memory.md)**.

### µwren

The gap on the small parts is not another feature flag. Dropping the compiler,
the transcendentals and 64-bit numbers is already possible — what is left is
**the core library itself**, and it is now measured rather than asserted.

**The crate is 111,640 B of flash** in a bytecode-only `-Os` build, the ESP
HAL excluded because a CH32 would not link it. Where that goes:

| | bytes | |
|---|---|---|
| the machine — interpreter 23,962, heap and collector 8,564, handles 3,470, loader 3,398, value/object/bytecode/symbol 2,622 | **42,016** | 38% |
| `Num` | 15,008 | 13% |
| `core::install` — the signature strings and the binding calls | 9,306 | 8% |
| `String` | 8,638 | 8% |
| `List` 6,016, `Map` 3,372, `Sequence` 3,008 | 12,396 | 11% |
| core helpers — indexing, bitwise, the hash probe | 2,912 | 3% |
| `random` 2,460, `meta` 746 | 3,206 | 3% |
| Rust monomorphisations — `BTreeMap<String, usize>` 1,320, two sorts 1,412, `str` pattern search 368 | 3,100 | 3% |
| `Range` 1,320, `Fiber` 1,018, `System` 858, `Object`/`Class` 752, `Bool`/`Null` 398, `Fn` 308 | 4,654 | 4% |

**The core library is 55 KB of the 111 KB — about half**, which is what the
paragraph above used to claim without a number behind it. The machine is the
other 42 KB, and a 62 KB part needs both to come down.

What the table says to remove, largest first:

- **`Num`, 15,008 B, and almost none of it is arithmetic.** The operators are
  a few hundred bytes; the bulk is `toString` — float-to-decimal formatting
  pulls in a large chunk of Rust's formatting machinery — plus parsing and the
  numeric conversions. An integer-only `toString` is the single biggest cut
  available anywhere in the crate.
- **`core::install`, 9,306 B**, is the signature strings and one `define` call
  per method. It shrinks in direct proportion to how many methods survive, so
  every class dropped below is paid twice.
- **`String`, 8,638 B** — interpolation, `split`, `indexOf`, the UTF-8 rules,
  and Rust's substring search with it. A node that formats one number wants a
  fraction of this.
- **`Map`, `Sequence` and `List` together, 12,396 B.** A program that reports a
  reading needs a list at most; the lazy `map`/`where`/`take`/`skip` protocol
  and a hash table are what a workstation language is for.
- **The `.wrenc` loader, 3,398 B**, is only needed to *parse* bytecode. A part
  that links one program at a fixed layout does not parse anything.
- **`random` and `meta`, 3,206 B**, are optional modules already built lazily.
- **The symbol table's `BTreeMap<String, usize>`, 1,320 B plus its sorts.**
  Method signatures are fixed once the core is installed, so a sorted `Vec` or
  a table computed at build time would remove the map and the string
  comparisons behind it.
- **`Fiber`, 1,018 B** plus the switch and park machinery inside the
  interpreter's 23,962 B.

That is a route to roughly 55 KB, which fits a CH32V006's flash and leaves
little for the program. Two changes go further, and both are changes to the
*language* rather than to this build of it — which is what makes µwren a
separate deliverable rather than a feature flag.

**No floating point at all — 34,654 B, and it is not in the figures above.**
Those counted `wren`-crate symbols; this is Rust's own `core`, linked because
Wren has one numeric type and it is an `f64`:

| | bytes |
|---|---|
| `dec2flt::POWER_OF_FIVE_128` — one table, for parsing decimals | 10,416 |
| Dragon and Grisu, shortest and exact formatting | 13,700 |
| `f64::from_str` | 3,712 |
| `CACHED_POW10`, `digits_to_dec_str`, the `fmt::float` entry points | 6,826 |

`--features f32` narrows this; it does not remove it, because an `f32` still
formats and still parses. An integer-only µwren removes all of it — and takes
the NaN tagging with it, because that representation *is* an `f64` bit
pattern, so the value type would become a tagged 32-bit word. Between this and
`Num`'s own 15,008 B, **arithmetic that is not floating point is the single
largest saving available**, worth more than the whole core library.

**And the compiler already knows what a program uses.** A `.wrenc` names every
method signature it calls and every class it touches — that is what the symbol
and variable tables in the file *are*. Emitting that as a manifest beside the
bytecode would let a firmware build link only the core it needs: `core::install`
is 9,306 B of signature strings and one binding call per method, and it shrinks
in exact proportion to what the manifest asks for. A program that never sorts a
list does not need `Sequence`; one that never interpolates does not need most of
`String`; and neither has to be decided by hand or by a cargo feature, because
the program already said.

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
| `binary_trees` depth 9 | 5.678 s | **2.160 s** | 4.729 s |
| `fib(24)` ×5 | 9.605 s | **3.250 s** | 7.109 s |
| `list_build` 10,000 | 0.420 s | **0.130 s** | 0.154 s |
| `method_call` | 1.601 s | **0.350 s** | 1.748 s |
| **VM resident** | **23,176 B** | 83,036 B | — |

**Between two and a half and four and a half times slower than C, and 72%
smaller resident.** Against MicroPython it is *slower* on three of the four and
faster on `method_call`; `--features nofp` closes most of that gap and still
does not overtake it on `list_build`. The speed is
the honest cost of reaching objects by a bounds-checked index rather than a
pointer, which is what keeps `unsafe` out of the object model; the memory is
what
compiling no core library at start-up buys.

**Judged by work, not only by time.** The chip's performance counter reports
instructions retired, which does not move when the code moves — so a change
that removes work can be told from one that merely landed better. Every change
here is decided that way, and rather more of them have been thrown out than
kept: the most recent pass over the interpreter kept nine and refused eleven,
with the numbers for all twenty in
**[`doc/wren-rs/profiling.md`](doc/wren-rs/profiling.md)**. Four of the
refusals removed a cost the compiler had already removed, and two removed a
test that was redundant as a predicate and load-bearing as a filter.

**Read the seconds to about two percent.** The board is exact -- the same image gives
the same time to the microsecond, three flashes running -- but *where the
instructions land* is worth more than that: padding every branch target to eight
bytes, with the source untouched, takes `fib` from 16.774 s to 16.127. So a
small difference between two builds is a fact about placement until it has been
measured at several placements, and consistency across all four benchmarks is
not the check it looks like. The experiment, the mirror one showing that moving
the *data* changes nothing at all, and the chip's performance counter that
settles both are in
**[`doc/wren-rs/profiling.md`](doc/wren-rs/profiling.md)**.

### The three numeric modes

`Num` is a double in Wren. `--features f32` makes it a single and `--features
nofp` makes it a 32-bit integer. Same board, same commit, same programs, one
run each:

| | `f64` | `f32` | `nofp` |
|---|---|---|---|
| `binary_trees` | 5.678 s | 5.295 s | **5.034 s** |
| `fib` ×5 | 9.605 s | 8.721 s | **7.480 s** |
| `list_build` | 0.420 s | 0.384 s | **0.248 s** |
| `method_call` | 1.601 s | 1.538 s | **1.440 s** |
| `binary_trees` peak | 115,168 B | 88,144 B | 92,080 B |
| `list_build` peak | 132,292 B | **66,656 B** | 66,636 B |
| VM resident | 23,176 B | 22,888 B | **21,316 B** |
| image | 472,928 B | 457,264 B | **400,512 B** |

Both narrower modes are faster *and* smaller, because `riscv32imac` has neither
the `F` nor the `D` extension — every width is software and the narrow ones are
less work. A `Value` halves to four bytes, which is why `list_build`, whose
whole memory is a list of numbers, halves with it.

**Neither is Wren, and the cost is exact rather than vague.** An `f32` is exact
on integers only to 2^24, so `list_build` — which sums to 49,995,000 — prints
**49,992,896**, and upstream's suite goes from 829 of 829 to **798**. `nofp`
prints 49,995,000 correctly, an `i32` having room for it, but has no fractions
at all and caps at 2^30. Both are off by default; conformance and every other
table here are `f64`.

`nofp` is what a part with no FPU and no room for one would run, and it is the
mode [`doc/wren_native.md`](doc/wren_native.md)'s tailored build uses.

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

*(The rows are a history and each was measured when it landed; the current
figures are the table above. Resident has since risen to 23,176 B, because a
class slot and a method table each grew a word to be able to point into flash
— which is what the tailored build below spends to save four times as much.)*

### Tailoring the VM to one program

A `.wrenc` file carries a manifest: every method signature its code can call
and every module variable it can name. That is enough to build a VM with only
the core those need, and enough for a build step to compute the resulting
classes, method tables and names *at image build time* and compile them in as
`static` data. `tools/make_native_executable.sh --uwren` does both.

`fib` on the same board, `-Os`, a fixed 16 KiB heap and no allocator under it:

| | tailored | full Wren, same board |
|---|---|---|
| the core's classes, tables and names in RAM | **0 B** | — |
| heap once the VM is built | **4,008 B** | 23,176 B resident |
| heap at the run's peak | **8,528 B** | — |
| `wren` in flash | **54,696 B** | 85,972 B |
| whole image | **171,728 B** | 472,928 B |
| `fib(24)` ×5 | 11.78 s | 9.605 s |

**Nothing of the core is in the heap.** Twenty-one classes, their flattened
method tables and their names all live in `.rodata`; what the heap holds is the
slots that address them, the VM's own vectors, and whatever the program
computes — 930 B of objects at peak.

**54,696 B fits a CH32V006's 63,488 B of flash**, which no build of this VM
managed before, and the 8,528 B heap peak is just over its 8,192 B of RAM —
with the `Vm` struct and the call stack still outside that figure.

The one row that goes the wrong way is the clock, and it is not the VM's doing:
this image is built `-Os` where the benchmark port is `-O3`. Integer
arithmetic is *faster* at equal optimisation — see the `nofp` column above.

How it is built, how to test it on the host without a board, and the one way to
get it wrong: **[`doc/wren_native.md`](doc/wren_native.md)**.

### Cooking a native executable

The same tailoring works for a host binary: the program is compiled to
bytecode, the core it needs is generated as Rust, both are compiled in, and the
result reads nothing from disk.

```sh
tools/make_native_executable.sh benchmarks/wren/fib.wren            # full Wren
tools/make_native_executable.sh benchmarks/wren/fib.wren --uwren    # tailored
tools/make_native_executable.sh doc/examples --uwren --run          # a folder
```

| on `fib` | full Wren | `--uwren` |
|---|---|---|
| executable | 760,512 B | **501,264 B** |
| bytecode compiled in | 589 B | 589 B |
| generated core | — | 5,732 B of Rust |

A third smaller, and most of what is left in either is the Rust standard
library that a firmware does not carry. It runs in 33 ms.

**Why it is worth having.** The failures that matter in a tailored build do not
look like failures: a method symbol numbered differently, or a frozen method
table indexed against the wrong install order, reaches a device as a wrong
answer and nothing else — and a flash-and-watch cycle is the better part of a
minute. The same program built for the host runs in milliseconds and fails in
exactly the same ways, because it is the same VM with the same features.
`cargo test --test frozen` is the narrow version: six checks on `fib` in about
a tenth of a second.

**The one way to get it wrong** is to generate a core with one feature set and
compile it with another. A frozen method entry holds a *primitive index*, and
those are positions in the sequence of `define` calls that the cargo features
decide — so mixing the two puts every method on a plausible, wrong primitive,
and the symbol table does not move when it happens. It has already happened
once here. The script generates the core with the features it is about to
compile, which is how using it avoids the problem; `FrozenCore::disagreement`
catches it at start-up if something else causes it.

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

## Layout

```
crates/wren/    the Rust VM -- the crate a firmware depends on
vendor/wren/    upstream wren-lang/wren, submodule, unmodified
benchmarks/     the same programs in both languages, same constants
doc/examples/   a worked node in Wren, and the bytecode built from it
doc/wren/       what upstream Wren and MicroPython measured
doc/wren-rs/    why the Rust implementation is built the way it is
doc/wren_native.md  building a program into a native executable
ports/          one directory per (board, implementation) pair
  esp32c6-wren/          the C reference
  esp32c6-micropython/   the baseline
  esp32c6-wren-rs/       the deliverable
  esp32c6-wrenc-rs/      the same benchmarks with no compiler linked
  esp32c6-wren-boot/     a node that looks for boot and main
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
| **the C reference on the C6** | **measured** |
| **the MicroPython baseline** | **measured** |
| **the Rust VM** | **the language is complete** |
| — upstream's test suite | **829 of 829** |
| — conformance probes | **96 of 96** |
| — this crate's own tests | 193 |
| **shipping bytecode** | **measured** |
| — the suite, run from `.wrenc` | **829 of 829** |
| — the suite in an `f32` build | 798 of 829 — *not Wren, and off by default* |

### The language is complete

`crates/wren` passes **all 829** of upstream Wren's own tests — every group,
including `core`, `language`, `limit`, `meta`, `random` and `regression` — run
unmodified and scored against the `// expect:` comments they already carry.
That is the same contract the C port was held to, where upstream
itself scored 821 of 846 on an ESP32-C6.

Everything the language has: classes with constructors, fields, inheritance,
`super`, static members and static fields; closures with upvalues; fibers as
real coroutines, and the error handling built on them; modules and `import`;
maps as a hash table; `Sequence` with lazy `map`/`where`/`take`/`skip`; class
attributes; string interpolation and Wren's UTF-8 rules.

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

### Three documents

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

**[`doc/wren-rs/profiling.md`](doc/wren-rs/profiling.md)** — how any of this is
measured, and what each instrument lies about. Why a workstation called every
real optimisation noise; a sampling profiler built out of `gdb` because there
is no `perf`, and the three things it gets wrong; the board being exact to the
microsecond while **3 to 4% of a benchmark is where the code landed**; and the
chip's performance counter, whose event numbers are in no header here and were
found by experiment — which then said the interpreter spends two cycles in
three retiring nothing.

### Shipping bytecode, measured

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

### The numbers to beat

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

### What the C reference established about the small parts

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
strongest argument yet for building bytecode on a host, so the part
never carries a compiler. And upstream's default garbage-collector thresholds
(10 MB before the first collection) mean that **out of the box it crashes on
this class of part**, with a null store rather than a diagnostic.

The lexer came first because it needs no hardware and is where the host tests
start. Everything else waits on a board.

## Licence

MIT.
