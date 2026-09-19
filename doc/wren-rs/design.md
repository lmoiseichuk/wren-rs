# How wren-rs represents values and objects

The decisions here are the ones that are expensive to change later, so they are
argued rather than asserted. Two constraints drive all of them:

1. **The API should be recognisable to someone who knows upstream Wren.** Same
   type names, same semantics, same observable behaviour — so that a difference
   is a bug against a known reference rather than a different language.
2. **The smallest target has 8 KB of RAM.** Upstream's VM is 83,036 B resident
   with a 33,552 B compiler stack, measured on the C6. That does not fit on a
   CH32V006 and no amount of tuning will make it.

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
| `ObjMap` | 12 B + entries | 28 B + entries |
| `ObjRange` | **24 B** | 32 B |
| `ObjClass` | 16 B | 40 B + method table |
| `ObjInstance` | 16 B + fields | 16 B + fields, inline |
| `MapEntry` | 16 B | 16 B |

Every slot in the table is one `Object`, so **every object occupies 24 bytes
before its contents** — the size of the largest variant.

`ObjRange` is that largest variant, at two `f64`s and a `bool`. Shrinking it
would buy nothing: its two doubles force 8-byte alignment on the whole enum, the
next largest variants are already 16 B, and a tag would round anything smaller
back up to 24. **24 B is the floor for this layout**, not an oversight.

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

**Every access is bounds-checked.** A pointer dereference becomes an index into
a `Vec`. Expect single-digit to low-double-digit percent, and note that the
benchmark set from steps 1 and 2 exists precisely so this can be answered with a
number rather than argued.

### The alternative that has not been built

One table per type, with the type in the handle's high bits. That removes the
max-variant waste — a `List` would cost 12 B instead of 24 — and makes type
checks free. It costs six free lists instead of one, six sweep loops, and a
handle encoding that constrains how many objects of each type can exist.

**It is not being built yet because the waste has not been shown to matter.**
On an 8 KB part with, say, 200 live objects, the difference is about 2.4 KB —
which on that part is not nothing. That is the measurement that would justify
it, and it needs the VM to exist before it can be taken.

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

`value.rs`, `object.rs` and `heap.rs`: the representation, the object types the
collector can meaningfully trace, and mark-sweep over them. The function, closure,
fiber and module types arrive with the compiler rather than as stubs now — a
half-populated struct that pretends to be a type is worse than an absent one.
