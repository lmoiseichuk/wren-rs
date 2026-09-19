# wren-rs on the ESP32-C6 — measured

ESP32-C6FH4 rev v0.2 @ 160 MHz, the same board that produced
[`benchmarks.md`](benchmarks.md). The same four programs from
`benchmarks/wren`, unchanged, each timing itself with `System.clock` exactly as
it does under the C port.

**The first measurement of this implementation was published untuned**, because
the result was not the one the design predicted and that is worth more than a
flattering number. It has since been worked on; both sets of numbers are below,
because the first one is what the second is owed to.

## Speed

| benchmark | wren-rs `speed` | wren-rs `size` | C Wren `-O2` | C Wren `-Os` | MicroPython | wren-rs vs C `-O2` |
|---|---|---|---|---|---|---|
| `binary_trees` | **8.268 s** | 15.371 s | 2.160 s | 2.440 s | 4.729 s | 3.8x slower |
| `fib` | **16.601 s** | 33.669 s | 3.250 s | 3.710 s | 7.109 s | 5.1x slower |
| `list_build` | **0.568 s** | 1.028 s | 0.130 s | 0.150 s | 0.154 s | 4.4x slower |
| `method_call` | **2.356 s** | 4.326 s | 0.350 s | 0.420 s | 1.748 s | 6.7x slower |

**wren-rs is four to seven times slower than upstream Wren on this part**, and
one and a half to five times slower than MicroPython. `method_call` is the
worst of the four, which is the tell: it is almost pure dispatch, and dispatch
is where this implementation's indirections all land. That contradicts what
`doc/wren-rs/design.md` predicted for the central design decision — reaching
objects by a bounds-checked index rather than by pointer — which it put at
"single-digit to low-double-digit percent". The measurement says otherwise, and
the design note has been corrected.

### What the first run said, and what changed

| benchmark | first run | now | |
|---|---|---|---|
| `binary_trees` | 8.616 s | 8.268 s | −4.0% |
| `fib` | 18.176 s | 16.601 s | −8.7% |
| `list_build` | 0.574 s | 0.568 s | −1.0% |
| `method_call` | 2.766 s | 2.356 s | **−14.8%** |

The gain is all in the call path, which is why `method_call` moves most and
`list_build` — a loop over a primitive — barely at all. Entering a call asked
the heap for the same two objects three times over, once for the arity, once
for the code and once for the module; returning did the walk again for the
caller's chunk and module; field access fetched its offset through closure and
function on every read and every write; and method lookup walked the superclass
chain, where upstream copies a parent's methods down when a class is created.
Ten heap lookups per call became four.

**None of this was visible on a workstation.** Every one of these changes
measured inside the noise there, because an out-of-order core hides a dependent
load that an in-order RISC-V pays for in full. Anyone repeating this should not
trust a laptop to say whether an indirection matters.

The optimiser matters far more here than it does for C: `size` against `speed`
is **2x**, where the C port's `-Os` against `-O2` is 13–20%. A VM written as
many small Rust functions depends on inlining in a way a C switch loop does
not, and `opt-level = "z"` declines to do it.

## Memory

The one clear win, and it is a large one:

| | wren-rs | C Wren |
|---|---|---|
| **VM resident after construction** | **24,680 B** | 83,036 B |

**The VM is 70% smaller than upstream's before a line of user code runs.** That
is the figure that decides whether a part is usable at all. Two things produce
it. The first is compiling no core library at start-up: upstream builds
`wren_core.wren` every time a VM is created, and this does not. The second was
found by counting rather than by reasoning — the method tables were the largest
thing on the heap, because a class's table is indexed by global method symbol
and is therefore as long as the highest symbol it answers to and almost all
empty. Handing back the `Vec` slack and packing an entry from eight bytes to
four took resident from 45,676 B through 49,004 B to 24,680 B.

Per-benchmark, measured the same way on both — free before minus free after,
no forced collection:

| benchmark | wren-rs | C Wren `-O2` | |
|---|---|---|---|
| `binary_trees` | **158,124 B** | 78,784 B | *2.0x more* |
| `fib` | **3,888 B** | 6,616 B | *1.7x less* |
| `list_build` | **132,460 B** | 134,712 B | *1.0x less* |
| `method_call` | **7,948 B** | 15,096 B | *1.9x less* |

**These are peaks, not live sets, and the difference is most of the number.**
Measured with the heap census (`bench --census`), `binary_trees` holds 76,727 B
live where the device peaks at 158,124 B — so **half of what it uses is
floating garbage** the growth threshold has not collected yet. That is a dial
rather than a fact: 1.25x instead of the default 1.5x took the peak to
160,628 B from 185,156 B at the time it was measured, for 4.6% more time.
`Heap::set_growth` exists for a firmware that wants to make that trade; the
default matches upstream so these columns stay comparable.

`binary_trees` is the outlier and the reason is known rather than guessed:
**every object here occupies 24 bytes before its contents**, the size of the
largest variant of one enum, where upstream allocates each type at its own
size. A tree of instances is exactly the workload that pays for it. The other
three are equal or better.

## Image size

**These are not comparable, and the difference is the platform rather than the
VM.** The C port is an ESP-IDF application carrying FreeRTOS, newlib and a
console; this port is bare metal carrying none of it. Two images of similar
total size can contain very different amounts of VM.

What *is* measurable on this side is how much the VM adds to an otherwise
identical firmware — the same runtime, allocator, printing and timer, with no
`wren` in it:

| | `size` | `speed` |
|---|---|---|
| full image | 278,144 B | 440,576 B |
| the same firmware with no VM | 111,728 B | 113,472 B |
| **the VM's own contribution** | **166,416 B** | **327,104 B** |

For the equivalent figure on the C side, an empty ESP-IDF application would
have to be built and subtracted the same way. Until that is done, the only
honest statement about image size is that the two numbers measure different
things.

## What is not controlled

Three things, stated because they bound what the speed figures prove:

* **The platforms differ.** Bare-metal esp-hal against ESP-IDF means the flash
  cache and MMU are configured by different code. Instruction fetch from
  memory-mapped flash is sensitive to that, and it has not been measured
  either way. It is a candidate for *part* of the gap and it has not been
  ruled out.
* **`Sequence` is Rust here and Wren upstream.** These four benchmarks use
  none of its methods -- they are loops, field access, arithmetic and dispatch
  -- so this comparison is unaffected. A benchmark that used `map` or `where`
  would be measuring that difference rather than the interpreter.
* **One run each.** The C port's figures are medians of three; these are
  single runs, and the device is deterministic enough that the wall clock and
  the program's own clock agree to a few parts in ten thousand, but that is
  not the same as a repeated measurement.

## Where the time is likely going

Named as hypotheses to test, not as conclusions:

* **A bounds-checked index per object access**, against a pointer dereference.
  This is the design's central trade and the first thing to measure.
* **Method lookup walks the superclass chain on every call**, with no inline
  cache. Upstream walks it too, but its per-class table is a direct array
  index into a flat method table.
* **A class is boxed**, so dispatch is one more indirection than upstream's.
* **The value stack is a `Vec` with bounds checks**, where upstream uses a raw
  pointer.

Each is a decision recorded in `doc/wren-rs/design.md` with a stated cost. The
costs were stated as small. One of them is not.
