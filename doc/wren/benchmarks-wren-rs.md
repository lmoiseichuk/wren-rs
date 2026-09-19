# wren-rs on the ESP32-C6 — measured

ESP32-C6FH4 rev v0.2 @ 160 MHz, the same board that produced
[`benchmarks.md`](benchmarks.md). The same four programs from
`benchmarks/wren`, unchanged, each timing itself with `System.clock` exactly as
it does under the C port.

**This is the first measurement of this implementation, not a tuned one.** It
is published because the result is not the one the design predicted, and that
is worth more than a flattering number.

## Speed

| benchmark | wren-rs `speed` | wren-rs `size` | C Wren `-O2` | C Wren `-Os` | MicroPython | wren-rs vs C `-O2` |
|---|---|---|---|---|---|---|
| `binary_trees` | **8.616 s** | 17.087 s | 2.160 s | 2.440 s | 4.729 s | 4.0x slower |
| `fib` | **18.176 s** | 34.921 s | 3.250 s | 3.710 s | 7.109 s | 5.6x slower |
| `list_build` | **0.574 s** | 1.028 s | 0.130 s | 0.150 s | 0.154 s | 4.4x slower |
| `method_call` | **2.766 s** | 4.810 s | 0.350 s | 0.420 s | 1.748 s | 7.9x slower |

**wren-rs is four to eight times slower than upstream Wren on this part**, and
one and a half to five times slower than MicroPython. `method_call` is the
worst of the four at 7.9x, which is the tell: it is almost pure dispatch, and
dispatch is where this implementation's indirections all land. That is the headline, and it
contradicts what `doc/wren-rs/design.md` predicted for the central design
decision — reaching objects by a bounds-checked index rather than by pointer —
which it put at "single-digit to low-double-digit percent". The measurement
says otherwise, and the design note has been corrected.

The optimiser matters far more here than it does for C: `size` against `speed`
is **2x**, where the C port's `-Os` against `-O2` is 13–20%. A VM written as
many small Rust functions depends on inlining in a way a C switch loop does
not, and `opt-level = "z"` declines to do it.

## Memory

The one clear win, and it is a large one:

| | wren-rs | C Wren |
|---|---|---|
| **VM resident after construction** | **45,676 B** | 83,036 B |

**The VM is 45% smaller than upstream's before a line of user code runs.** That
is the figure that decides whether a part is usable at all, and it is the
direct result of compiling no core library at start-up: upstream builds
`wren_core.wren` every time a VM is created, and this does not.

Per-benchmark, measured the same way on both — free before minus free after,
no forced collection:

| benchmark | wren-rs | C Wren `-O2` | |
|---|---|---|---|
| `binary_trees` | **160,244 B** | 78,784 B | *2.0x more* |
| `fib` | **4,120 B** | 6,616 B | *1.6x less* |
| `list_build` | **132,412 B** | 134,712 B | *1.0x less* |
| `method_call` | **9,800 B** | 15,096 B | *1.5x less* |

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
| full image | 273,376 B | 428,144 B |
| the same firmware with no VM | 111,728 B | 113,472 B |
| **the VM's own contribution** | **161,648 B** | **314,672 B** |

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
