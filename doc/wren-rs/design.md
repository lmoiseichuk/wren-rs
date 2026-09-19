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

There is also a cost refcounting looked certain not to escape: Wren has cycles
by construction. A class refers to its methods, a method's closure refers to
its module, a module refers to the class. A pure refcount leaks all of that.

**That turned out to be true and irrelevant**, which is the sort of thing only
a measurement finds: those cycles are in the *live* set, and a collection never
sees them. The sections below are what happened when the table above was
stopped being argued about and built instead.

### What should replace it — what the profile says

`cargo run --release --features profile --example heap-profile` runs all 873
programs in the repository and counts four things that were being argued from
first principles. Two of the arguments were wrong.

**Nothing dominates the population.**

| type | share of allocations | slot B | own-table B |
|---|---|---|---|
| `List` | 21.5% | 2,752,584 | 1,376,292 |
| `Closure` | 19.7% | 2,518,104 | 419,684 |
| `Upvalue` | 19.2% | 2,458,872 | 1,639,248 |
| `Instance` | 19.0% | 2,434,776 | 1,623,184 |
| `String` | 13.3% | 1,707,480 | 1,138,320 |
| `Class` | 6.6% | 847,320 | 141,220 |
| everything else | 0.8% | | |

Per-type tables would save **50.3% of the slot bytes** on allocation volume,
against 35% on the live snapshot measured earlier — because `Closure`, `Class`,
`Fn` and `Fiber` are four bytes of payload sitting in a 24-byte slot, and
together they are 27% of what gets allocated. That is now built; see **One
table per type** above for what it measured on the board, which was less than
this number and came with a speed gain the number did not predict.

**Mark-sweep is expensive in exactly one place.**

| benchmark | collector's share of runtime | marked per object freed |
|---|---|---|
| `binary_trees` | **16.9%** | 1.70 |
| `list_build` | 0.6% | 34.2 |
| `method_call` | 0.1% | 29.0 |
| `fib` | 0.0% | 19.8 |

So "mark-sweep is slow" is true for a program that allocates and false for one
that does not, which is what one would hope — but the second column is the
interesting one and it goes the other way. **Where the collector is cheap it is
also wildly inefficient**: `list_build` traces 34 objects to reclaim one,
because its live set is large and stable and every survivor is marked again at
every later collection. Survival across all 873 programs is 52.9%, and 95–97%
for the three benchmarks that hold something.

That waste is precisely what a generational collector removes. **It is also
where the collector already costs nothing**, which looked at the time like the
argument against building one: on `binary_trees`, the one program where
collection is 17% of the clock, tracing is already efficient at 1.70.

**That reasoning used the wrong number and the conclusion was wrong.** Survival
of the whole live set is not the generational hypothesis; survival of
*recently allocated* objects is, and they are not the same thing at all. Asked
properly -- of the objects allocated since the last collection, how many are
alive at the next one -- the answer is:

| | allocated | survive |
|---|---|---|
| `binary_trees` | 100,183 | **16.2%** |
| all 873 programs | 533,645 | **18.6%** |

So 84% of what the one allocating benchmark produces is dead by the next
collection, and a nursery would reclaim it while tracing only the sixth that
is not. The other three benchmarks allocate about a hundred objects each, so
their survival percentages are noise rather than counter-evidence.

**No garbage was cyclic. None.** Across 453,581 garbage objects, a simulated
reference count — built at every collection by counting references within the
garbage and removing whatever falls to zero — would have freed **100%** of it
the moment it died.

This document asserted the opposite, and both statements are true: Wren does
have cycles by construction, a class refers to its metaclass and a closure to
its module, but those are in the *live* set and a collection never sees them.
What actually becomes garbage in these programs is trees, strings, lists and
closures, and none of it points back at itself.

A zero is worth nothing until the instrument has been shown to report
non-zero, so the profiler first runs a program built to make 250 cyclic objects
and prints what the simulation says about it. It says 250.

### What was recommended, before any of it was built

The profile's first reading said: refcounting in front, tracing behind,
because 100% of the garbage 873 programs produce is acyclic and half of peak
looked like floating garbage. Both halves of that were built. The sections
below are what they measured, kept in the order they were learned because the
order is the useful part.

### What refcounting measured, and what it means for a nursery

Refcounting is built, behind the `refcount` feature, and it is off. The
barriers were proved complete rather than audited -- `verify_counts` walks
every live object, counts the references that exist, and reports every count
that disagrees; it started at roughly twenty thousand and reports none across
all 873 programs. With it on, **95.8% of reclaims never reach the collector.**

It still does not pay:

| | time | `binary_trees` peak |
|---|---|---|
| tracing only | 8.169 s | 133,888 B |
| counting only | 8.980 s | 154,064 B |
| counting and freeing | 9.630 s | 126,056 B |

Eighteen percent of the one allocating benchmark to save six percent of its
peak, and three to five percent of the others to save nothing.

**Why, stated so it is not relearned.** The profile says the collector is
16.9% of `binary_trees` and 0.0–0.6% of the rest, so anything replacing it
wins at most 17% of one program. Counting, by contrast, costs work on every
store whether or not anything is reclaimed — the cost scales with what the
program *does*, while the saving is capped by what the collector *costs*.

And the "half of peak is floating garbage" figure that motivated it compared a
host census of the live set against the device's retained heap. Those are not
the same measurement.

### The nursery, built and measured

Built, behind the `nursery` feature, and off. Objects already live in slots
addressed by a stable index, so **a generation is a bitmap over slots rather
than a region of memory**: a survivor is promoted by clearing a bit and never
changes address. The migration a copying nursery pays for does not arise.

A minor collection traces the roots and the remembered set, does not scan old
objects, frees the young slots nothing reached and promotes the rest. The
invariant it rests on -- an old object may only point at a young one if the
barrier recorded it -- is checked the same way the reference counts were, by
`verify_remembered`, and holds across all 873 programs.

**On the host it does exactly what the theory says.** For `binary_trees`:

| nursery | collector's share | promoted | major collections |
|---|---|---|---|
| none | 28.2% | — | 139 |
| 256 objects | 19.7% | 56% | 65 |
| 512 | 16.6% | 46% | 49 |
| 1024 | 10.8% | 24% | 26 |
| 2048 | **7.8%** | **13.7%** | **14** |

Major collections fall by an order of magnitude and the promotion rate
converges on the 16% the profile predicted.

**On the device it is slower and much larger.** With a 2048-object nursery and
a 16 KB headroom: 9.105 s against 8.455, and a peak of 176,736 B against
133,880. Two reasons, and the second is the one that matters:

- A minor collection's fixed costs -- clearing ten mark bitmaps, gathering the
  roots, clearing ten young and ten remembered bitmaps -- are proportional to
  the whole table rather than to the nursery, so on a part where the collector
  was only 17% to begin with they eat the saving.
- **The nursery is peak memory.** Peak is the live set, plus the headroom the
  old generation is allowed, plus the nursery. A 2048-object nursery is about
  60 KB of young objects that by construction are not collected yet. Shrinking
  it to bound that pushes the promotion rate back up -- 46% at 512 -- and
  promoted garbage can only be reclaimed by a major.

That trade has no good point on this workload, which is the finding. Without a
headroom it does not merely lose: majors become rare enough that the old
generation grows past the 320 KB the part has, and it runs out of memory.

### Three alternatives, one conclusion

Reference counting, a young generation, and a fixed headroom have all now been
built and measured against the tracing collector. On an ESP32-C6FH4 at
160 MHz, `binary_trees`, speed profile:

| | time | peak |
|---|---|---|
| **tracing, as it stands** | **8.455 s** | 133,880 B |
| tracing + 16 KB headroom | 9.240 s | **121,624 B** |
| reference counting | 9.630 s | 126,056 B |
| nursery + 16 KB headroom | 9.105 s | 176,736 B |

**Nothing beats the collector that is already there on time, and the simplest
thing beats everything on memory.** The profile said why before any of it was
written: the collector is 16.9% of the one benchmark that allocates and
0.0-0.6% of the rest, so a replacement can win at most 17% of one program --
while every replacement adds work proportional to what the program *does*
rather than to what the collector *costs*.

The useful part of all three is what they leave behind: a write barrier at
every store into a heap object, verified complete two different ways, and a
profiler that can price the next idea before it is built.

### The dial that is already there

How far the live set may grow before collecting again. Upstream's default is
1.5x and this matches it, for comparability rather than because it is right on
a part with 320 KB.

Measured on an ESP32-C6, 1.25x instead took `binary_trees` peak down 13% for
4.6% more time, and did not move the other three benchmarks at all, because
they do not collect often enough for the threshold to matter.

**A ratio is the wrong shape for a fixed heap, though.** It lets a program hold
half as much garbage again as it is using, so the allowance grows with the
workload on a part whose total does not. `Heap::set_headroom` makes the
threshold the live set plus a constant instead: with 16 KB of headroom,
`binary_trees` peaked at 121,624 B rather than 133,880 — nine percent less for
eight percent more time, and at a number a firmware can budget against.

Of everything tried against the collector, this is the only one that stayed
useful, and it is four lines.

### Instance fields in chunks

The census priced the allocator's own overhead at 9,312 B across
`binary_trees`, and almost all of it was instances: a `Vec` per object means a
call to the allocator per object, with a header and a rounding to match --
a thousand instances, a thousand blocks, eight kilobytes of headers for
twenty-four kilobytes of fields.

They are in chunks the heap owns now, and an instance records where.
`Option<ObjInstance>` falls from 16 bytes to 12 on the way, because the start
index is biased by one and that gives `Option` a niche.

**Three things about the shape, each of which was learned by getting it
wrong:**

- **Chunks, not one arena.** The first attempt was a single `Vec`, and a `Vec`
  doubles: it asked the allocator for a 128 KB block while still holding the
  64 KB one and ran out of memory on a 320 KB part, holding a hundred
  kilobytes of fields. Growth is one chunk at a time and a run never straddles
  two.
- **The size adapts** -- 32 values, then 128, 256, 512. Four kilobytes is a
  good block for a program holding thousands of instances and absurd for
  `method_call`, which allocates two in its life and paid a whole chunk for
  them. The offset lives in the low sixteen bits of the start index, which is
  what leaves the sizes free to differ.
- **One spare chunk is kept, not one per chunk in use.** Keeping a spare for
  every live chunk simply doubled the arena, which is worse than the
  fragmentation it was avoiding.

Compaction closes the holes a sweep leaves, **in place and in source order**: a
run only ever moves to an address at or below where it was, so copying them in
increasing order of where they are cannot overwrite one that has not been
copied yet. Building a fresh set of chunks would hold two copies of the arena
at once, which is the spike the whole structure exists to avoid.

Measured on the board, `binary_trees` peak heap 133,880 → **117,384 B**. It
costs 7.7% of that benchmark: one extra bounds check per field access, chunk
then offset, where there used to be one index into a vector the instance
owned.

### Where the memory went, end to end

`binary_trees` on an ESP32-C6FH4 at 160 MHz, from the first run on hardware:

| | peak heap | VM resident |
|---|---|---|
| first hardware run | 160,244 B | 45,676 B |
| packed method tables, `Vec` slack returned | 158,124 B | 24,680 B |
| one table per type | 133,888 B | 22,920 B |
| instance fields in chunks | **117,384 B** | 22,920 B |
| …and a 16 KB ceiling on garbage | **109,004 B** | 22,920 B |

**A third off the peak and a half off resident**, and every step of it was
chosen by the profiler rather than guessed at. The two that were guessed at --
refcounting and a nursery -- are the two that are switched off.

### What is left worth building

Not another liveness policy: three have been measured and the collector wins.
What the numbers still point at is **where objects live, not when they die**:

- **Chunked slot tables.** The *fields* are chunked; the slots themselves are
  not. A table's `Vec` still holds its high-water mark for ever, so a program
  that spikes never gives that back. The same structure applies, and the field
  arena is the worked example of what it costs and what it is worth.

  **A chunk could be an object itself** -- a hidden type the language never
  sees, holding a block of slots and addressed by a handle like anything else.
  The attraction is that chunk lifetime then reuses the machinery that already
  exists: the tables, the free list, the sweep. The cost is a bootstrap, since
  the table that holds chunks cannot itself live in a chunk, and a second
  indirection on every access -- handle to chunk, chunk to slot -- which is
  the one thing this design has spent the most effort removing. Worth
  measuring before it is assumed either way; a plain `Vec` of boxed blocks per
  type needs no bootstrap and no extra hop.
- **An occupancy bitmap instead of `Option` in every slot**, for anything
  added later whose payload has no spare bit pattern. `ObjUpvalue` was that
  case -- 24 bytes for a 16-byte payload, on 19.2% of all allocations -- and
  biasing its stack slot by one gave it a `NonZeroU32` and the niche. The next
  type without one will not necessarily have a field to bias.

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

What this document is now for is the record of which decisions were tested and
what they measured: the object cost table, the census, the indirection count,
the three replacements for the collector that were built and are switched off,
and the two things still worth building. The negative results are the most
useful part of it — each one was predicted by a number already on this page
and read wrongly, and the reading is recorded beside the correction.
