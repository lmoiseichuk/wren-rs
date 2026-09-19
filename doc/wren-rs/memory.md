# Memory: how it is organised, and what reclaims it

Where objects live, what they cost, and how they are freed — in the order it
was learned. This is the long form; [`design.md`](design.md) carries the
decisions and the summary table.

Two threads run through it, and they turned out to be the same subject:

- **Where the memory actually goes.** Counted rather than guessed, and it was
  never where the argument had been looking: not the objects but their method
  tables, then the allocator's own per-object headers.
- **What reclaims it.** Mark-sweep, and the three replacements that were built,
  machine-verified and rejected.

**Each replacement was justified by a number already on this page, and each of
those numbers had been read wrongly.** The readings are recorded beside the
corrections, because a correction is cheaper to reuse than the experiment that
produced it.

---

## Why the collector is replaceable, and what replacing it would involve

The intended future directions are reference counting — easy to replicate, and
predictable in a way a stop-the-world pause is not — and something closer to
Rust's own ownership discipline. Two of the three have since been built and
measured; what made that possible is the boundary described here.

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

### The ceiling on garbage — the one that stayed

How far the live set may grow before collecting again. Upstream's default is
1.5x and this matches it, so the published numbers stay comparable with the C
port.

**A ratio is the wrong shape for a fixed heap.** It lets a program hold half as
much garbage again as it is using, so the allowance grows with the workload on
a part whose total does not. `Heap::set_headroom` makes the threshold the live
set plus a constant instead, and it means exactly that: `INITIAL_THRESHOLD`,
the 4 KB floor that stops a three-object program collecting immediately, does
not apply to it. Four kilobytes is half a CH32V006's entire RAM and would
quietly swallow any ceiling smaller than itself.

Measured on an ESP32-C6FH4 at 160 MHz, speed profile:

| ceiling | `binary_trees` | its peak | `method_call` |
|---|---|---|---|
| ratio, 1.5x | 9.106 s | 117,376 B | 2.5185 s |
| 32 KB | 8.970 s | 121,480 B | 2.5185 s |
| 16 KB | 10.148 s | 109,004 B | 2.5185 s |
| 8 KB | 12.211 s | 109,004 B | 2.5185 s |
| 4 KB | 16.545 s | 109,000 B | 2.5185 s |
| **1 KB** | 37.916 s | **83,148 B** | **2.5185 s** |

**The last column is the point.** `method_call` holds about a hundred objects
and its time does not move at all -- not to five decimal places -- from the
loosest setting to the tightest. `binary_trees` holds a thousand-node tree and
pays four times over for the same ceiling.

That is not a coincidence, it is what tracing costs: **a collection is
proportional to the live set**, so tightening the ceiling multiplies a cost
that is already near zero when there is little to trace. The parts that most
need a tight bound -- a CH32V006 with 8 KB, an X035 with 20 KB, where a program
holds tens of objects rather than thousands -- are precisely the ones where
tightening it is free.

Two other things the curve says:

- **Between 16 KB and 4 KB the peak barely moves** (109,004 → 109,000) while
  the time doubles. Below 16 KB the peak is no longer set by floating garbage
  but by the live set plus the field chunks and the tables' high-water mark, so
  there is nothing left for the ceiling to squeeze. Only at 1 KB, where
  collection is near-continuous, does it fall again -- to 83,148 B, 29% under
  the ratio.
- **32 KB is slightly *faster* than the ratio and slightly larger.** At that
  size the ceiling is looser than 1.5x of this live set, so it collects less.

So the setting is a real choice rather than a default to inherit: a firmware
that knows its live set can pick the ceiling that fits its RAM, and on a small
part it can pick a very tight one and pay almost nothing.

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

