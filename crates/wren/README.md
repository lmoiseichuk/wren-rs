# wren

[Wren](https://wren.io/), re-implemented in Rust for microcontrollers.

Wren is a small class-based scripting language with a bytecode VM, closures,
fibers and a garbage collector. This crate re-implements it to run on parts with
kilobytes rather than megabytes — `no_std`, no dependencies, no `unsafe`.

```toml
[dependencies]
wren = "0.1"
```

> **Status: early.** The lexer, the value representation, the object model and a
> mark-sweep collector over it are written and tested. The compiler and the
> interpreter loop are not. It does not yet run Wren programs.

## The design, in short

Two constraints decide everything: the API should be recognisable to someone who
knows upstream Wren, and **the smallest target has 8 KB of RAM**. Upstream's VM
is 83,036 B resident with a 33,552 B compiler stack, measured on an ESP32-C6.
That does not fit on a CH32V006 and no amount of tuning will make it.

### Values are NaN-tagged, in safe Rust

A `Value` is 8 bytes: an `f64` whose NaN payload carries everything that is not a
number. The constants are upstream's, so the two can be read side by side.

The usual objection is that NaN tagging needs `unsafe` pointer punning. It does
not here: `f64::to_bits` is safe, and the payload is a 32-bit table index rather
than a pointer. **The crate is `#![forbid(unsafe_code)]`.**

The alternative — a tagged enum — is 16 bytes, and doubling every stack slot,
list element and instance field is not affordable on these parts.

### Objects are reached by handle, and carry no header at all

**This is the key departure from upstream, and the feature most of the rest
follows from.** Upstream's `Value` holds an `Obj*`, and every object carries a
16-byte header: its type, a mark bit, its class, and a `next` pointer threading
it onto a global list the collector walks.

Here a `Value` holds an `ObjectId(u32)` indexing one table the heap owns, and
**an object's header is zero bytes**:

| upstream, per object | here |
|---|---|
| `ObjType type` — 4 B | the enum discriminant, folded into padding |
| `bool isDark` — 4 B after alignment | one bit in a bitmap beside the table |
| `Obj* next` — 4 B | absent; the table *is* the list |
| `ObjClass* classObj` — 4 B | implied by the variant ([a debt](#what-this-costs)) |
| **16 B** | **0 B** |

Three things fall out of it, and they are why the representation is worth the
departure:

* **No `unsafe`.** A pointer-based object graph with a tracing collector needs
  `unsafe` throughout, or `Rc`/`RefCell` overhead on every object. An index
  sidesteps the question — and a handle to a collected object is a failed
  lookup returning `None`, never a read of freed memory.
* **A sweep that scans words, not pointers.** Marks in a side bitmap mean the
  collector walks 64 bits of mark at a time instead of chasing a linked list
  through objects scattered across the heap. On a part with no cache worth the
  name, that is the larger win.
* **The collector is replaceable.** Every reference between objects is a handle
  into one table, so reclamation policy is confined to `heap.rs`. Nothing else
  in the VM learns how an object's lifetime is decided.

### What this costs

Set out plainly, because a design note that only lists wins is not one:

* **Every object occupies 24 bytes before its contents** — the largest variant
  of the object enum, set by `Range`'s two `f64`s. Upstream allocates each type
  at its own size. A `List` needs 12 B and gets 24.
* **Variable-length data is a second allocation.** Upstream inlines a string's
  bytes and an instance's fields in the object's own allocation; here they are a
  `Vec`, with its own allocator overhead and one more indirection.
* **Every access is bounds-checked** rather than a pointer dereference. Expect
  single-digit to low-double-digit percent; it is not yet measured.
* **No `classObj`.** Enough for what is built, but a debt against the dispatch
  path when foreign classes and built-in subclassing arrive.

All the numbers above are measured by compiling for `riscv32imac-unknown-none-elf`
and reading the sizes back, and pinned by assertions in `object.rs` so the
documentation cannot quietly drift from the layout.

## On replacing the collector

Mark-sweep today. Reference counting is the obvious next thing to try — easy to
replicate, and predictable in a way a stop-the-world pause is not — and the
handle table is what would make it tractable.

**It would not be a drop-in, and it is worth knowing why before starting.**
Refcounting needs to observe every copy and every death of a handle, and `Value`
is `Copy` precisely so the interpreter loop can move it without ceremony. Making
it non-`Copy` so `Drop` can fire is the change, and it ripples through the whole
interpreter.

There is also a cost refcounting does not escape: **Wren has cycles by
construction.** A class refers to its methods, a method's closure refers to its
module, the module refers back to the class. A pure refcount leaks all of that,
so a cycle collector comes back anyway — and the comparison worth running is
against mark-sweep-with-a-nursery, not against nothing. The test suite has both
cycle cases, reachable and unreachable, for when somebody tries.

## Features

| feature | default | what it is for |
|---|---|---|
| `std` | yes | the host: tests, and an ahead-of-time compiler |
| `alloc` | via `std` | the object heap. Without it you get the lexer |

A firmware build is `default-features = false`, usually with `alloc`.

## Measured against

Upstream Wren 0.4.0 and MicroPython 1.29, both on an ESP32-C6, same programs and
same constants. Those numbers are the target this is being built to beat, and
they are in [`doc/wren/benchmarks.md`](../../doc/wren/benchmarks.md) in the
repository.

## Licence

MIT, as upstream Wren.
