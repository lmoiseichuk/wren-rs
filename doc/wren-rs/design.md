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
and — see below — the payload is a 32-bit index rather than a pointer. The crate
keeps `#![forbid(unsafe_code)]`.

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

### The alternative that has not been built

One table per type, with the type in the handle's high bits. That removes the
max-variant waste — a `List` would cost 12 B instead of 24 — and makes type
checks free.

**The measurement this section used to ask for has now been taken**, and it
came out smaller than expected. Pricing every live object in `binary_trees` at
its own size rather than at the largest variant saves 9,644 B of the 27,096 B
of slots — 35% of the slots, but only **9% of the 104,575 B live heap**, because
the slots were never where the memory was. The method tables alone were four
times that, and the floating garbage four times again.

So it is still not built, and now for a reason with a number attached rather
than for want of one. Its better argument is no longer memory but dispatch:
`class_of` is a heap lookup today, and a type in the handle's high bits would
make it free for every built-in. That is the version worth building, and it
should be judged on the benchmark clock rather than on the census.

## Why the collector is replaceable, and what replacing it would involve

The intended future directions are reference counting — easy to replicate, and
predictable in a way a stop-the-world pause is not — and something closer to
Rust's own ownership discipline. Neither is being built now. What is being built
now is the boundary they would need.

Because every reference between objects is an `ObjectId` and every object lives
in one table the heap owns, **reclamation policy is confined to `heap.rs`**. The
rest of the VM asks for an object by handle and never learns how its lifetime is
decided. Concretely:

| strategy | what changes in `heap.rs` | what changes elsewhere |
|---|---|---|
| mark-sweep *(now)* | `collect(roots)` walks and frees | nothing |
| reference counting | `retain`/`release` on handle copy; a slot's count replaces its mark bit | **every place a `Value` is copied or dropped** |
| ownership-flavoured | a move-or-borrow discipline on handles | the compiler, which would have to prove it |

The middle row is the honest one: refcounting is *not* free to drop in, because
the thing it needs — knowing when a handle is copied and when it dies — is
exactly what a `Copy` `Value` does not tell you. Making `Value` non-`Copy` so
that `Drop` can fire is the change that would be required, and it would ripple
through the interpreter loop. **The handle table makes it possible; it does not
make it cheap.** Writing that down now is better than discovering it later.

There is also a cost refcounting does not escape: Wren has cycles by
construction. A class refers to its methods, a method's closure refers to its
module, a module refers to the class. A pure refcount leaks all of that, so a
cycle collector comes back anyway, and the comparison to run is against
mark-sweep-with-a-nursery rather than against nothing.

### What refcounting would actually be worth here

The census gives the prize a number for the first time. `binary_trees` is
104,575 B live and peaked at 185,156 B on the device, so **43% of peak is
garbage the threshold is holding rather than anything the representation
wastes**. That is larger than every object-layout saving in this document put
together, and it is what prompt reclamation would recover.

Two things make it more tractable than the table above suggests:

**Deferred counting keeps `Value` `Copy`.** Count only references held *by heap
objects* — an instance's fields, a list's elements, a map's entries, a closed
upvalue — and never those on the stack or in locals. Then the interpreter's
hottest path is untouched and only `StoreField` and the container mutations
adjust a count. An object reaching zero becomes a *candidate* rather than
provably dead, because the stack may still hold it, and candidates are confirmed
by a scan of the roots that are already enumerated for the collector.

**The counters cost nothing per object.** They go in a side array parallel to
the slots, exactly as the mark bits already do — which is what preserves the
zero-byte object header this design is built around. A `u8` is 1 B per slot,
about 1.1 KB at the live counts measured here. A count that saturates at 255
sticks there and is never decremented again, so such an object can only be
freed by tracing.

That saturation is the second reason mark-sweep stays underneath, alongside
cycles. The end state is not "refcounting instead" but **refcounting in front,
tracing behind** — and the tracing half is then run rarely rather than on a
growth threshold.

### The dial that is already there

Until then, the same 43% has a one-line lever: how far the live set may grow
before collecting again. Upstream's default is 1.5x and this matches it, for
comparability rather than because it is right on a part with 320 KB.

Measured on an ESP32-C6, 1.25x instead took `binary_trees` peak from 185,156 B
to 160,628 B — **13% less memory for 4.6% more time** — and did not move the
other three benchmarks at all, because they do not collect often enough for the
threshold to matter. `Heap::set_growth` exists so a firmware can make that
trade; the default is left alone so the published numbers stay comparable with
the C port.

## The memory targets these have to meet

| part | RAM | flash | what is plausible |
|---|---|---|---|
| CH32V006 | 8 KB | 62 KB | bytecode only — no compiler on the part |
| CH32X035 | 20 KB | 62 KB | bytecode only, a larger heap |
| ESP32-C6 | 512 KB | 4 MB | everything, including the compiler |

The 8 KB row is why the crate is split so the compiler is optional: upstream
spends 33 KB of stack compiling its own core library before user code runs, and
that alone is four times a CH32V006's entire RAM. **Step 4 of the plan — compile
to bytecode on a host — is not an optimisation, it is the only way the small
parts are reachable at all.**

## What is built so far

All of it: the representation, the object types, mark-sweep over them, the
lexer, the compiler, the interpreter and the core library. `crates/wren` passes
all 829 of upstream's own tests, from source and through a bytecode round-trip.

What this document is now for is the record of which representation decisions
were tested and what they measured — the object cost table, the census, the
indirection count, and the two levers that have numbers but no implementation
yet (per-type tables, and refcounting in front of the tracing collector).
