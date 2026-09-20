# How wren-rs represents values and objects

The decisions here are the ones that are expensive to change later, so they are
argued rather than asserted. Two constraints drive all of them:

1. **The API should be recognisable to someone who knows upstream Wren.** Same
   type names, same semantics, same observable behaviour — so that a difference
   is a bug against a known reference rather than a different language.
2. **The smallest target has 8 KB of RAM.** Upstream's VM is 83,036 B resident
   with a 33,552 B compiler stack, measured on the C6. That does not fit on a
   CH32V006 and no amount of tuning will make it.

This one is **24,680 B resident** on the same part, measured the same way — 70%
under upstream. Most of that gap is two decisions argued below: compiling no
core library at start-up, and an object that carries no header.

## What a measurement on this board is worth

Two facts govern every number below, and both cost a day to learn. They are
argued in full in [`profiling.md`](profiling.md); the short form is:

**The board is exact.** Three flashes of one image give `binary_trees`
9.006731 s, 9.006731 s and 9.006731 s. One run is the measurement.

**Its code placement is not.** Padding every branch target to four bytes --
same source, same structs, only the instructions moved -- is worth 3 to 4% on
all four benchmarks. So a difference of one or two per cent between two builds
says nothing about the change, and *consistency across all four benchmarks says
nothing either*, because that is exactly what placement produces. The chip's
performance counter is what settles those: instructions retired move by one
part in 541 million across a placement change worth 3% of the time.

The interpreter is **fetch-bound** — between 2.6 and 2.8 cycles per instruction
on an in-order core whose common instructions take one. That is the fact behind
the next section, and behind the padding now being in the build.

### What it said about the obvious next optimisation

The interpreter decodes each byte into an `Op` and matches on the `Op` -- a
switch feeding a switch, which the sampling profiler put at 14% of
`binary_trees` and which LLVM does not fuse on this target. Matching the raw
byte instead, with `const u8` patterns, makes it one jump table. Measured at
several placements, both variants built from the same commit:

| | `binary_trees` | `fib` | `method_call` | flashed image |
|---|---|---|---|---|
| two tables, no padding | 9.007 s | 16.774 s | 2.479 s | 465,904 B |
| one table, no padding | 8.787 s | 16.273 s | 2.395 s | 465,552 B |
| two tables, 4 B padding | **8.729 s** | **16.127 s** | **2.382 s** | 472,544 B |
| one table, 4 B padding | 8.824 s | 16.380 s | 2.422 s | 472,192 B |

**The two are substitutes, and together they are worse than padding alone.**
The rewrite is worth 2.4-3.4% on an unpadded build and is a 1.1-1.6%
*regression* on a padded one. Both treat the same bottleneck, and once it is
relieved the rewrite only adds code. So it is not in the tree: it would have to
be re-argued against whatever placement ships, and it costs the exhaustiveness
`match op` gives for free -- a new opcode with no arm would become a runtime
"bad opcode" rather than a compile error.

*The shape of that finding is the one to carry forward: on this part a source
change to the interpreter competes with instruction placement rather than
adding to it, and neither can be judged without the other.*


## Value: NaN tagging, as upstream, in safe Rust

A `Value` is 8 bytes: an `f64` whose NaN payload carries everything that is not
a number. This is upstream's `WREN_NAN_TAGGING` representation and the constants
are deliberately the same ones (`SIGN_BIT`, `QNAN`, the singleton tags), so the
two can be read side by side.

The alternative — a tagged `enum { Null, Bool(bool), Num(f64), Obj(ObjectId) }` —
is 16 bytes, because the `f64` forces 8-byte alignment and the discriminant then
costs a whole word. **Doubling the size of every stack slot, every list element
and every instance field is not affordable on these parts**, and upstream reached
the same conclusion for the same reason.

The usual objection to NaN tagging is that it needs `unsafe` pointer punning.
It does not here, for two reasons: `f64::to_bits` and `f64::from_bits` are safe,
and — see below — the payload is a 32-bit index rather than a pointer. So the
value representation needs no `unsafe`, and neither does the object model built
on it. The crate is `#![deny(unsafe_code)]` rather than `forbid`, because the
interpreter's fetch lifts it; nothing here does.

**What this costs.** Wren has a single numeric type, so every number is an
`f64`. On RV32IMAC with no FPU that is soft-float, and the C6 measurements show
what it costs: Wren beat MicroPython's tagged small integers on `fib` anyway,
but by 2.19x rather than the ~5x it manages on `method_call`, which is pure
dispatch. Keeping upstream's semantics means keeping that cost. A `f32` or
small-integer fast path would break compatibility with every Wren program that
relies on 53-bit integer precision, so it is not on the table as a default.

## Objects: a handle into a table, not a pointer

Upstream `Value` holds an `Obj*`, and every object carries a header threading it
onto a global linked list the collector walks. Here, a `Value` holds an
`ObjectId(u32)` indexing a slab the heap owns.

This is the one place the implementation deliberately departs from upstream's
shape, so it is set out in full.

### The handle

```
ObjectId(u32)   -- a plain index into the heap's table. No type bits.
```

Four bytes, which is what a pointer is on the 32-bit parts this targets, so the
indirection costs nothing in space — only a bounds check on each access.

**No type tag is packed into the handle**, though there is room for one. It
would make `is_string(value)` answerable without touching the table, which is
tempting. It is left out because the type is already free once the object is
loaded — it is the enum discriminant — and because reserving bits now would
constrain the table size for a saving that has not been shown to matter. If the
interpreter loop turns out to type-check far more often than it dereferences,
that is a measurement that would change this.

### What one object costs

Measured by compiling for `riscv32imac-unknown-none-elf` and reading the sizes
back, not estimated:

| type | payload | upstream, same target |
|---|---|---|
| `ObjString` | 16 B + bytes | 24 B + bytes, inline |
| `ObjList` | 12 B + elements | 28 B + elements |
| `ObjMap` | 16 B + entries | 28 B + entries |
| `ObjUpvalue` | 16 B | 24 B |
| `ObjRange` | **24 B** | 32 B |
| `ObjClass` | 16 B | 40 B + method table |
| method table entry | **4 B** | 8 B |
| `ObjInstance` | 16 B + fields | 16 B + fields, inline |
| `MapEntry` | 16 B | 16 B |

Every slot in the table is one `Object`, so **every object occupies 24 bytes
before its contents** — the size of the largest variant.

`ObjRange` is that largest variant, at two `f64`s and a `bool`. Shrinking it
would buy nothing: its two doubles force 8-byte alignment on the whole enum, the
next largest variants are already 16 B, and a tag would round anything smaller
back up to 24. **24 B is the floor for this layout**, not an oversight.

That was a prediction, so it was tested rather than left standing: `Range` was
boxed, which takes the largest payload down to 16 B, and `Object` was still
24 B. The remaining 16-byte variants each hold a `Value`, which is 8-byte
aligned, so the tag rounds back up exactly as the paragraph above says. The
experiment was reverted -- it cost an allocation and an indirection per range
for nothing.

Getting below 24 needs the alignment gone, and that means `Value` no longer
being 8-byte aligned. The only route to that is a 32-bit `Value`, which means
32-bit floats. **That is built** -- the `f32` feature -- and it does reach
20 bytes a slot, by removing the alignment rather than the payload. See
**Numbers as `f32`** below.

It is a floor that has to be defended, though. `ObjUpvalue` originally stored
`Option<Value>` for "closed, and here is the value" — the obvious spelling, and
eight bytes larger than a `Value`, because a `Value` has no spare bit pattern
for `None` and the discriminant needs a word of its own. That tied `Range` at 24
and pushed the enum to 32, which is **eight bytes on every object in the heap**
to express one bit about upvalues. It stores `undefined` instead, a value no
Wren program can hold, which is what that singleton is for.

### Where the memory actually goes

Predicting this was wrong twice, so it is now counted. `cargo run --release
--example bench -- --census` reports live objects by type, what their own `Vec`s
hold, and what the allocator charges in headers. For `binary_trees` on the host,
whose object graph is the same shape as the device's:

| | at first measurement | now |
|---|---|---|
| slots | 27,096 B | 27,096 B |
| contents | 68,167 B | 40,311 B |
| allocator headers | 9,312 B | 9,320 B |
| **live heap** | **104,575 B** | **76,727 B** |

Two findings came out of it, neither of which was where the argument had been
looking:

**The method tables were the largest single item, not the instances.** A class's
table is indexed by *global* method symbol, so it is as long as the highest
symbol that class answers to and almost all of it is empty. Across 42 live
classes that was 42,832 B against 24,552 B for a thousand instances. Two changes
halved it and then halved it again:

- The tables are built by repeated `resize`, and a `Vec` that grows by doubling
  ends up holding about half as much again as it uses. That slack -- 14,560 B,
  invisible to `len` and visible only in `capacity` -- is now handed back by
  `shrink_to_fit`, in the sweep and once after the core library is installed. A
  class is settled by then: Wren cannot add a method after the body has run.
- An entry was an `Option<Method>` at 8 B, of which 4 B was a tag and its
  padding. It is now a packed `u32`. Paging the symbol space was measured
  against this and saved slightly more -- 45% against 50% -- but would have put
  a second dependent load on every call, so it was not taken.

**Most of the peak is not in the live set at all.** The device peaked at
185,156 B against 104,575 B live, so **43% of peak was floating garbage** held
by the growth threshold rather than anything an object representation could fix.
See the collector section below.

### The header upstream pays and this does not

```c
struct sObj {            // 16 bytes on a 32-bit part
  ObjType type;          //  4  -- here: the enum discriminant, in Range's padding
  bool isDark;           //  1 (+3 pad) -- here: a bit in a side bitmap
  ObjClass* classObj;    //  4  -- here: absent, see below
  struct sObj* next;     //  4  -- here: absent, the table *is* the list
};
```

**Upstream spends 16 bytes per object on a header; this spends zero.** Three
separate reasons, each worth having explicitly:

* The **type tag** costs nothing because Rust folds the enum discriminant into
  `ObjRange`'s trailing padding. This is luck rather than design, but it is
  pinned by an assertion in `object.rs` so it cannot quietly stop being true.
* The **mark bit** lives in a bitmap beside the table rather than on the object.
  That removes a byte (really four, after alignment) from every object, and it
  makes the sweep a scan of 64-bit words rather than a pointer chase through
  objects scattered across the heap — which on a part with no cache to speak of
  is the larger of the two wins.
* The **`next` pointer** is not needed because the table already enumerates
  every object. Upstream needs the list precisely because its objects are
  scattered; ours are not.

The one upstream field with no counterpart here is **`classObj`**. Upstream
stores each object's class so that method dispatch on a built-in works the same
way as on an instance. Here the class of a string, list, map or range is implied
by its variant, and `ObjInstance` carries its own `class` handle. That is enough
for everything built so far, but **it is a debt against the dispatch path**: when
foreign classes arrive, and when a program subclasses a built-in, this will need
revisiting. Better to record it now than to discover it as a missing feature.

### The table itself

```
slots:  Vec<Option<Object>>   -- 24 B per slot, full or empty
free:   Vec<u32>              --  4 B per *currently free* slot
marks:  Vec<u64>              --  1 bit per slot
```

`Option<Object>` is also 24 bytes — the enum's spare discriminants give it a
niche — so **an empty slot costs exactly what a full one does and not a word
more.** That too is pinned by an assertion, because losing it silently would add
a word to every slot in the table.

The free list is a separate stack of indices rather than a chain threaded
through the empty slots. Threading is the traditional trick and costs no extra
memory, but expressing it in Rust means either `unsafe` or an awkward dance with
indices stored in a variant that exists only to hold them. A `Vec<u32>` that is
only as long as the number of free slots is the smaller idea, and free slots are
transient.

### Where this is worse than upstream

Honesty about the two places it loses:

**Variable-length data is a second allocation.** Upstream puts a string's bytes
and an instance's fields *inside* the same allocation as the object, with a
flexible array member. Here they are a `Vec`, which is a separate allocation with
its own allocator overhead and an extra indirection to reach. For a short string
the header saving roughly cancels the extra allocation; for many short strings,
allocator overhead is real and this will show up.

**Every access is bounds-checked — and it costs far more than this note used to
claim.** A pointer dereference becomes an index into a `Vec`. The estimate here
was "single-digit to low-double-digit percent". The first run on real hardware
says **four to eight times slower than upstream Wren**, worst on the benchmark
that is almost pure method dispatch — see
[`../wren/benchmarks-wren-rs.md`](../wren/benchmarks-wren-rs.md).

Worth being precise about why the estimate was wrong, rather than just
correcting the number. It was reasoned about as one bounds check per *field
access*. It is not: it is a check on every traversal of every reference, and a
single method call makes several — receiver to class, class through its box,
class to method table, method to closure, closure to function, function to
chunk. Upstream follows a pointer at each of those. Counting one of them and
calling it representative is the mistake.

**Counting them turned out to be worth doing, because several were redundant.**
Entering a call asked the heap for the same two objects three separate times —
`function_of` for the arity, `chunk_of` for the code, `module_of` for the
namespace — six lookups to read three fields that sit beside each other in one
`ObjFn`. Returning did the same walk again to find the caller's chunk and
module. Field access fetched its offset through closure and function on *every*
read and *every* write. And method lookup walked the superclass chain, where
upstream copies a parent's methods into the child when the class is created.

What is left after fixing those is four lookups per call where there were ten,
and the measured result on an ESP32-C6 at 160 MHz was `method_call` 2.766 →
2.356 s, `fib` 18.176 → 16.601, `binary_trees` 8.616 → 8.268, `list_build`
0.574 → 0.568.

*Each table in this document measures one change against the commit before it,
so the absolute numbers are of that moment rather than of today; the current
figures are in [`../wren/benchmarks-wren-rs.md`](../wren/benchmarks-wren-rs.md).
What a change was worth does not move when a later one lands, which is why they
are recorded this way.*

**The host said none of it.** Measured on a workstation, every one of those
changes was inside the noise: an out-of-order core hides a dependent load that
an in-order RISC-V pays for in full. Anyone repeating this work should not trust
a laptop to tell them whether an indirection matters.

The decision may still be right for a crate that forbids `unsafe`. But it has
to be argued against what is left of the gap, not against the first number, and
the honest form of that argument is a measurement of each indirection rather
than a prediction about them.

### Numbers as `f32`

Wren's `Num` is a double, and that is a language guarantee rather than an
implementation choice — but on a part with no FPU it is an expensive one. This
is now built, behind the `f32` feature, off by default. **With it on, this is
not Wren.**

Measured on an ESP32-C6FH4 at 160 MHz, running bytecode with no compiler
linked, `f64` against `f32`:

| | `-Os` | `-O3` |
|---|---|---|
| `binary_trees` | 14.172 → 13.042 s | 7.882 → 7.015 s |
| `fib` | 32.642 → 29.739 s | 16.737 → 14.638 s |
| `list_build` | 0.992 → 0.891 s | 0.570 → 0.495 s |
| `method_call` | 4.183 → 3.914 s | 2.382 → 2.128 s |
| image | 244,416 → 227,184 B | 360,816 → 341,056 B |
| `binary_trees` heap | 155,676 → 120,772 B | same |
| `list_build` heap | 132,096 → 66,508 B | same |

Six to thirteen percent faster, 17–20 KB smaller, and a fifth to a half off the
heap. `riscv32imac` has neither the `F` nor the `D` extension, so both widths
are software and the narrower one is simply less work.

**The layout, which is the part this document is for.** A `Value` is 4 bytes,
so every stack slot, list element, instance field and map entry halves —
`list_build`'s heap halving is a list of 10,000 numbers and nothing else.
`ObjRange` becomes 12 B rather than 24, and the enum's alignment drops from 8
to 4, so the largest payload of 16 plus a tag rounds to **20 bytes a slot
instead of 24**. That is the floor this document said could only be reached
this way, and it was.

**It is not a retype.** NaN tagging depends on the mantissa. A double leaves 52
bits and a handle needs 32, so nothing is lost; a single leaves 23, of which
`QNAN` spends two. So the encoding is redone at the narrower width and a handle
gets **21 bits — 2,097,151 objects**, against the roughly sixteen thousand a
320 KB heap holds at 20 bytes each. `Value::object` asserts it in debug builds
rather than silently aliasing two objects onto one handle.

**It changes answers, and the cost is exact rather than vague.** An `f32` holds
integers exactly only to 2^24, so `list_build` — which sums to 49,995,000 —
prints 49,992,896. Upstream's suite goes from 829 of 829 to **798**, every
failure a precision one. Conformance runs and every published benchmark stay on
`f64`.

Three things needed real work rather than a type swap, and they are the ones
worth knowing about before turning this on for something else:

- **`Random` stored its four `u32` state words one per field.** A `Num` holds
  one exactly only when it is a double, so at the narrow width the state would
  have been rounded on every step and the generator would have collapsed. Each
  word is now kept as two 16-bit halves, which both widths hold exactly.
- **Two hash folds shifted a number's bits right by 32**, which is an overflow
  when those bits are 32 wide. Both widen first, so the fold is a no-op rather
  than a panic.
- **Formatting drops from fourteen significant digits to eight.** Seven would
  send any integer above 9,999,999 into exponential form while an `f32` still
  holds integers exactly to 16,777,216; nine would stop `0.1` printing as
  `0.1`. Eight is the value that keeps both.

The `.wrenc` format is unchanged and stores a number as a double whatever the
build is: bytecode is produced on a workstation and read on a part, and the two
need not agree about the width.

### One table per type — built, and what it measured

The original note called this "the alternative that has not been built" and
left it for want of evidence. The heap profile supplied the evidence and it is
now built.

Every object used to cost 24 bytes before its contents, because every slot was
one `Object` and an enum is as large as its largest variant: a `List` needs
twelve and was charged twenty-four, and a boxed `Class` was charged
twenty-four for a four-byte pointer. There are now ten tables, one per type,
each with its own free list and mark bits, and a slot costs what that type
costs.

**The type moved into the handle** — four bits, and the *low* four, because a
packed method table entry reserves the top bit and an `f32` build has only 21
bits of payload to hold a whole handle in. Low bits leave both alone and simply
shorten the index.

Measured on an ESP32-C6FH4 at 160 MHz, against the commit before it:

| | before | after |
|---|---|---|
| `binary_trees` peak heap | 158,124 B | **133,888 B** |
| VM resident | 24,680 B | **22,920 B** |
| `binary_trees` | 8.268 s | **8.169 s** |
| `fib` | 16.601 s | **16.573 s** |
| `list_build` | 0.568 s | **0.562 s** |
| `method_call` | 2.356 s | **2.348 s** |

**It pays twice, which was not the expectation.** The memory was the point;
the speed came from `class_of`, where seven of ten answers are now the
handle's four bits and no heap access at all. That is on the dispatch path, so
it runs on every method call.

Getting that second half needed one distinction worth recording. `type_of`
answers "what is this, if it is still there", which means reading the type's
table to see whether the slot is occupied. The first cut used it in `class_of`
and measured **2–4% slower** — the heap read it was supposed to remove was
still happening, plus a tag check. Splitting off `kind_of`, which reads the
handle and nothing else, turned that into 1% faster. The dispatch path does
not need to know whether an object is live; it is holding a reference to it.

The fixed cost is about 400 bytes of `Vec` headers for ten tables instead of
one, which is why `fib` and `method_call` gained a few hundred bytes each while
`binary_trees` saved twenty-four kilobytes.

**It saved 34% of the slot bytes, not the 50% the census predicted**, and the
gap is worth naming because it is the next thing to fix. A slot is
`size_of::<Option<T>>()`, and `Option` needs a spare bit pattern or it costs a
whole word:

| | payload | slot | why |
|---|---|---|---|
| `List` | 12 | 12 | the `Vec` pointer is the niche |
| `Closure` | 16 | 16 | same |
| `Upvalue` | 16 | **24** | a `usize` and a `Value`, neither with a spare pattern |
| `Range` | 24 | 24 | unchanged, and it is 0.05% of allocations |
| `Class`, `Fn`, `Fiber` | 48–56 | 4 | still boxed; the slot holds the pointer |

`Upvalue` is **19.2% of everything allocated** and got nothing out of this. An
occupancy bitmap beside each table instead of an `Option` in every slot would
take it to 16 and cost one bit, and would do the same for anything else added
later that has no niche.

Two smaller things fell out of the same work:

- **A closure lives in its slot now.** `ObjClosure` is a handle and a vector,
  16 bytes, which fits inside the 24 that `ObjRange` already forces — so
  boxing it cost an allocation, a header and an indirection for nothing, on
  19.7% of everything the VM allocates. `Class`, `Fn` and `Fiber` are 48 to 56
  bytes and stay boxed.
- **`Object` survives only as the argument to `allocate`.** Nothing stores one.
  Its size assertions still pin the enum, but the enum is now a constructor's
  parameter rather than the shape of the heap.

## Reclamation, in one table

Mark-sweep, as upstream, with the marks in a side bitmap rather than a header
bit. **Three replacements were built and measured and none of them stayed** --
reference counting, a young generation, and a fixed ceiling on garbage. On an
ESP32-C6FH4 at 160 MHz, `binary_trees`, speed profile:

| | time | peak |
|---|---|---|
| **tracing, as it stands** | **9.104 s** | 117,384 B |
| tracing + a 16 KB ceiling | 10.148 s | **109,004 B** |
| deferred reference counting | 9.630 s | 126,056 B |
| a young generation + that ceiling | 9.105 s | 176,736 B |

The profile said why in advance: collection is 16.9% of the one benchmark that
allocates and 0.0-0.6% of the other three, so a replacement can win at most 17%
of one program -- while each of these adds work proportional to what the
program *does* rather than to what the collector *costs*.

Refcounting is behind `--features refcount` and a nursery behind
`--features nursery`; both are correct, both have machine-verified write
barriers, and both are off. `Heap::set_headroom` is what stayed: it makes peak
memory *the live set plus a constant* rather than half as much again as the
live set, which is the shape a fixed heap wants.

**Why the policy is confined to `heap.rs`.** Every reference between objects is
an `ObjectId` and every object lives in a table the heap owns, so nothing
outside that file learns how a lifetime is decided. That is what let three
collectors be tried without the VM noticing.

Everything behind those numbers -- the heap census, what each replacement cost
and why, the field chunks, the ceiling on garbage and what is left worth
building -- is in [`memory.md`](memory.md).

## The memory targets these have to meet

| part | RAM | flash | what is plausible |
|---|---|---|---|
| CH32V006 | 8 KB | 62 KB | not as it stands — see below |
| CH32X035 | 20 KB | 62 KB | not as it stands — see below |
| ESP32-C6 | 512 KB | 4 MB | everything, including the compiler |

The 8 KB row is why the crate is split so the compiler is optional: upstream
spends 33 KB of stack compiling its own core library before user code runs, and
that alone is four times a CH32V006's entire RAM. **Compiling to bytecode on a
host is not an optimisation, it is the only way the small parts are reachable
at all.**

**And it is not enough on its own.** This VM is 22,920 B resident before a line
of user code, which does not fit a V006 and leaves an X035 almost nothing. What
would have to change is not another feature flag but the core library itself:
the classes, their method tables and the primitives behind them are most of
that figure. A part that size wants a **µwren** — a deliberately reduced
language with a fraction of the core library, built with the linker set to
discard everything it does not reach. That is a separate deliverable from this
one, and pretending otherwise would put a number in the table that has never
been measured.

The CH32V003, at 2 KB, is not a target for this at all.

## What is built so far

All of it: the representation, the object types, mark-sweep over them, the
lexer, the compiler, the interpreter and the core library. `crates/wren` passes
all 829 of upstream's own tests, from source and through a bytecode round-trip.

What this document is now for is the record of which decisions were tested and
what they measured: the object cost table, the census, the indirection count,
the three replacements for the collector that were built and are switched off,
and the two things still worth building. The negative results are the most
useful part of it — each one was predicted by a number already on this page
and read wrongly, and the reading is recorded beside the correction.
