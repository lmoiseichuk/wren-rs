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

### What was left worth building, built

All three are now settled by measurement rather than argument.

#### Chunked slot tables — built, measured, behind a feature

`--features blocked-slots`. A table's slots live in blocks; a block goes back
to the allocator when the last slot in it is freed, and blocks past the end of
what the table addresses are dropped for good after a collection.

**A flat table can only grow.** Freeing a slot returns it to a free list, not
to the allocator, so a program that builds something large and drops it keeps
the high-water mark for the rest of its life. On a device that runs for months
after booting, that is the difference that matters -- and the *peak* is
unchanged either way, because at the peak every slot is in use.

| | flat | blocked, 16 | |
|---|---|---|---|
| `binary_trees` peak | 115,016 B | **87,872 B** | **−23.6%** |
| `fib` peak | 3,476 B | 4,116 B | +18.4% |
| `method_call` peak | 7,484 B | 8,612 B | +15.1% |
| `list_build` peak | 132,288 B | 133,248 B | +0.7% |
| work, all four | — | — | **+7 to +14%** |

**One benchmark a quarter better and three worse**, because a block is some
number of slots across ten tables and a small program never fills one; and
every object access pays an indirection. So it is off by default and on for a
program shaped like `binary_trees`: something large built and then let go, on
a part where the peak is what runs out.

##### How big a block, measured

`Heap::set_slot_block` takes the size at start-up, so this has an answer per
program rather than per build. Peak heap at every size, on the ESP32-C6:

| block | `binary_trees` | `fib` | `list_build` | `method_call` |
|---|---|---|---|---|
| flat | 115,016 B | **3,476 B** | **132,288 B** | **7,484 B** |
| 4 | 96,592 B | 3,716 B | 132,656 B | 7,932 B |
| 8 | 90,672 B | 3,956 B | 132,896 B | 8,172 B |
| **16** | 87,872 B | 4,116 B | 133,248 B | 8,612 B |
| 32 | **87,632 B** | 4,244 B | 134,208 B | 9,324 B |
| 64 | 89,680 B | 5,012 B | 136,128 B | 11,244 B |
| 128 | 92,880 B | 6,548 B | 139,968 B | 15,084 B |
| 256 | 101,904 B | 9,620 B | 147,648 B | 22,764 B |
| 512 | 114,100 B | 15,764 B | 163,000 B | 38,124 B |
| 1024 | 150,916 B | 28,052 B | *out of memory* | — |

And what the same sweep cost in work -- instructions retired, in millions,
which is the figure that does not move with code placement:

| block | `binary_trees` | `fib` | `list_build` | `method_call` |
|---|---|---|---|---|
| flat | **467.2** | **766.7** | **29.1** | **116.4** |
| 4 | 535.6 | 856.34 | 31.063 | 132.706 |
| 8 | 529.4 | 856.34 | 31.062 | 132.701 |
| **16** | **521.8** | 856.35 | 31.065 | 132.707 |
| 32 | 523.6 | 856.35 | 31.066 | 132.708 |
| 64 | 530.0 | 856.35 | 31.066 | 132.709 |
| 128 | 543.1 | 856.35 | 31.067 | 132.710 |
| 256 | 566.5 | 856.35 | 31.069 | 132.711 |
| 512 | 623.7 | 856.35 | 31.071 | 132.714 |
| 1024 | 629.1 | 856.36 | — | — |

**Three of the four columns are flat to five figures**, and that is the point
of them: the indirection on every object access costs what it costs, and no
block size buys any of it back. `binary_trees` is the exception because it is
the only one that makes and drops blocks rather than merely reading through
them, and the block is what it pays for -- 521.8M at 16 against 623.7M at
512, a fifth more work for the same program.

**Only `binary_trees` has an interior optimum**, because it is the only one
that frees in bulk and so the only one a larger block helps: a bigger block
recovers more of the tree when it is dropped, until the rounding across ten
tables costs more than it recovers. Its minimum is at 32, and 256 gives back
14 KB *less* than 32 does. For the other three every doubling is a straight
loss, and `list_build` is worse than flat at every size -- it builds one list
and holds it to the end, so there is nothing to give back and the block tail
is pure overhead.

**16 is the default, and the hardcoded 32 it replaces was a better guess than
it looked.** 32 was written before any of this was measured and is within 0.3%
of the peak optimum. 16 wins the default because it is the same number for
`binary_trees` -- 240 B worse on peak, but the *fastest* and the least work of
any blocked size, 8.47 s and 521.8M instructions against 8.50 s and 523.6M --
and strictly better than 32 on the other three benchmarks.

**Block size was expected to trade only memory, and mostly it does.** `fib`
retires 856.34M instructions at every size from 4 to 512: the indirection on
each object access costs the same whatever the block is. But `binary_trees`
moves, from 521.8M at 16 to 623.7M at 512, because it is the one that actually
makes and drops blocks, and a 512-slot block is 12 KB to fill with `None` and
hand back every time round.

**And 1024 ended the program.** `list_build` panicked with *memory allocation
of 131,072 bytes failed* -- its own element vector doubling to 128 KB, which
fits at every smaller block size and does not once ten tables are rounding up
to 1024 slots each.

##### What the right size depends on, which is not only this VM

**The allocator underneath decides as much as the workload does.** `esp-alloc`
is a first-fit free list with no size classes, so a large block is one large
request that either fits or ends the program -- which is exactly how 1024
died. Under an allocator with size classes and arenas -- jemalloc, ptmalloc,
tcmalloc -- a request that size is routine, and the same sweep would bottom
out somewhere else. This table measures a block size *on this allocator, for
objects of these sizes*, and neither half of that travels.

So read the number as a range to start from, not as a constant:

| part | sensible range |
|---|---|
| ESP32-class, hundreds of KB | 16–64 |
| CH32-class, a few KB of RAM | 8–16 |
| unknown allocator, be careful | 4–32 |

**32, 64 and 128 are a `binary_trees` answer, not a general one.** All three
sit near that benchmark's optimum, and all three are worse than 16 for every
other program measured here -- `method_call` pays 8,612 B at 16 and 15,084 B
at 128, for a program that holds a few dozen objects. A firmware that does not
build and drop a large structure should take the small end of its range; one
that does not do it at all should leave the feature off.

*Two attempts were needed. The first dropped a freed block's indices from the
free list, which abandoned the other slots in it for ever -- the tables
addressed 34,272 slots where they had held 2,560, and nothing was given back
at all. The free list has to keep them: an allocation landing on one makes the
block again, and until one does the memory is back.*

#### A chunk as a hidden object — not built, and the measurement says why

This page proposed it as the alternative to the above and answered itself: *"a
plain `Vec` of boxed blocks per type needs no bootstrap and no extra hop."*
That is what was built. The hidden-object form would add a second indirection
-- handle to chunk, chunk to slot -- on top of the one that already costs 6 to
9%, to save a `Vec` of pointers per type. There is nothing in the numbers to
pay for it.

#### An occupancy bitmap — not built, and a test that says when to

It was proposed *for a type added later whose payload has no spare bit
pattern*. There is no such type: `Option<T>` is the same size as `T` for all
ten, so the `Option` is free and a bitmap would cost a bit per slot and a test
on every access to save nothing.

`ObjUpvalue` is the one that nearly was not -- 24 bytes for a 16-byte payload,
on 19.2% of all allocations, until biasing its stack slot by one gave it a
`NonZeroU32`. So the useful form of this idea is a guard:
`every_slot_type_costs_nothing_for_being_optional` in `tests/heap.rs` fails on
the day someone adds a type without a niche, and says in its message that this
is when the bitmap is worth building. It demonstrates its own check can fail,
so it is not a test that always passes.

### Leaving the unused core out of the image

A `.wrenc` file carries a symbol table, and that table *is* a manifest: it lists
every signature the program will ever send. `fib` asks for ten core methods.
Everything else in the core -- `String`'s search and slicing, `List`'s sort and
insert, `Sequence`'s whole iterator protocol, `Map` -- is built at start-up,
occupies flash for its code and heap for its `ObjFn`s, and is then never called.

Two ways to not pay for it, and they save different things:

  - **`Vm::with_core_methods(&manifest.signatures)`** filters `define` at
    run time. The code is still in flash; the methods are never allocated, so
    it saves heap.
  - **Cargo features** leave the installers out of the build entirely. That
    saves flash, and the heap saving comes with it.

Both are in. The features are `str_extras`, `str_views`, `list_extras`,
`num_extras`, `sequence` and `map`, with `core_full` turning on all six and
carried by the crate's default. Measured on `ports/esp32c6-wrenc-rs`'s `uwren`
binary, which is `fib` and nothing else:

| build | `wren` in flash | whole image |
|---|---|---|
| `--features uwren,core_full` | 83,296 B | 189,008 B |
| `--features uwren` | **52,638 B** | **161,872 B** |
| saving | 30,658 B | 27,136 B |

**52,638 B fits a CH32V006's 63,488 B of flash**, which is the first time any
build of this VM has. The crate figure sums the symbols mangled into the `wren`
crate; attributing generic instantiations more loosely puts it 16,784 B higher,
so the *delta* is the trustworthy half of that column and the absolute is a
floor. The image figure needs no board -- `espflash save-image --chip esp32c6`
writes it to a file.

The reduced core still runs: `fib` prints 46368 and the suite is unchanged,
because the suite builds with `core_full`.

#### What a fixed heap then says about RAM

`uheap` is the other half -- a fixed buffer with no `alloc` under it, so the
peak it reports is the whole RAM requirement rather than a residual. Running
`fib` through it with the reduced core, at a 16 KiB `HEAP_BYTES`:

| moment | used | peak |
|---|---|---|
| after the VM is built | 7,296 B | 7,300 B |
| after the program loads | 8,284 B | 8,548 B |
| after the run | 11,464 B | 11,464 B |

Of which 2,785 B is objects; 185 block records are another 740 B. The core
split took the VM's resident set from 8,428 B to 7,296 B, and the run adds
3.2 KB on top: `fib` recurses, the `stack` and `frames` vectors double as they
grow, and a doubling allocates the new block before releasing the old -- which
`uheap` will not split, so each one leaves a hole behind.

**So flash fits a CH32V006 and RAM does not**, by 3,272 bytes against its 8,192.

#### What the 7,296 bytes actually are

`--features census` makes the VM answer that itself, on the part, and the
answer is not what the shape of the problem suggested. Built with `fib`'s
manifest, before a line of its code runs:

| | bytes |
|---|---|
| object contents -- strings, method tables | 1,667 |
| slot tables: `Class` 128, `String` 512 | 640 |
| slot bookkeeping -- free lists, mark bits | 64 |
| `primitives` | 256 |
| `method_names`, the interned signatures | 289 |
| `modules` -- the core namespace's names and values | 703 |
| **everything the VM asked for** | **3,619** |
| the runtime, before the VM existed | 544 |
| the census's own vectors | 326 |
| **everything every caller asked for** | **4,489** |
| the arena's own overhead | 2,235 |
| block records, 143 x 4 B | 572 |
| **the arena holds** | **7,296** |

and 1,480 bytes of `Vm` struct beside it, on the stack rather than in the heap.

The ledger closes to the byte, which is the point of printing it that way: a
term nobody thought of shows up as a discrepancy instead of hiding inside a
plausible figure. Two readings come out of it.

**The tailored VM is genuinely small -- 3,619 bytes -- and the allocator is
taking another 2,807 on top of it.** Overhead plus block records is 38% of the
total, more than every VM structure except the objects themselves.

**And that overhead has one cause, not three.** The census counts the two
mechanisms apart: alignment gaps absorbed by a fresh cut have cost 4 bytes in
the whole run, and holes reused whole have cost 4,144. `alloc_aligned` takes a
best-fit hole without splitting it -- the note there says splitting would put
the allocated half above the free remainder and break the descending order the
table depends on -- so a 64-byte hole answers a 12-byte ask and keeps the other
52. With 72 of the VM's 139 blocks at 16 bytes or under, that rounding falls on
nearly everything.

Which makes the next measurement a different one from the one the previous
section pointed at: splitting a reused hole, at the cost of teaching the table
an ascending pair, is worth about 2 KB of the 3,272 the part is short by. The
`stack` and `frames` vectors doubling during the run are the rest of it, and
still worth reserving from the manifest -- but they are no longer the larger
half.


### What is left worth building

Nothing from the list that used to stand here: chunked slot tables are built
and behind a feature, the hidden-object form is answered by their cost, and
the occupancy bitmap has a test that will say when its day comes.

What the numbers point at now is not the heap's shape but its contents.
`binary_trees` holds about two thousand live instances of three fields each,
and at eight bytes a `Value` that is 49 KB of the 115 KB peak -- so the
largest single lever left on memory is the one already measured and already
optional: `--features f32` halves every field, every list element and every
map entry, for a `Num` that is no longer Wren's. See the f32 table in the
README.

On a part small enough for the fixed heap above to be the whole of RAM, the
lever is a different one and it is named at the end of the previous section:
the `stack` and `frames` vectors doubling their way to a size a manifest could
have told them at start-up.
