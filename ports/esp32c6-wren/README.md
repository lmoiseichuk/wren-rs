# esp32c6-wren — the reference

Upstream Wren 0.4.0, in C, unmodified, running on an ESP32-C6 over USB
Serial/JTAG.

**This is the control, not a product.** It exists so that every later claim
about the Rust implementation is a difference measured against the same
language on the same silicon.

## Status: running

```
tools/flash.sh esp32c6-wren            # build and flash
tools/flash.sh esp32c6-wren --monitor  # and stay attached
```

Console commands: `.bench`, `.mem`, `.stack`, `.help`. Anything else is
evaluated as Wren.

## Results — 2026-09-18, ESP32-C6FH4 rev v0.2 @ 160 MHz, IDF v5.5, `-Os`

### Footprint

| | |
|---|---|
| image | **274 KB** (`0x43120`) |
| heap free at boot | 385,684 B |
| **VM resident** | **83,008 B** |
| **compiler stack** | **33,456 B** |

### Benchmarks

| benchmark | time | peak heap | what it exercises |
|---|---|---|---|
| `fib` | **752 ms** | 90,048 B | method dispatch, arithmetic |
| `tree` | **1,898 ms** | 180,116 B | allocation and the collector |
| `loop` | **1,901 ms** | 86,420 B | interpreter dispatch, soft float |

`fib` is `Fib.of(24)`; `tree` builds a depth-10 tree forty times; `loop` is
200,000 iterations of `x = x + i % 7`. Source is in `main/main.c` so it travels
with the numbers.

## How this differs

Four places where Wren as published did not work here. **None is a change to the
submodule** — the first is a build flag, the rest are configuration or port
code. Each is a requirement the Rust implementation inherits.

### 1. It does not compile under IDF's warnings

IDF builds with `-Werror=all -Wextra`. Two become errors:

* `wren_compiler.c:4127` — `%.*s` takes its precision as `int`; the argument is
  `int32_t`, which is `long int` on this target. A genuine portability bug that
  is invisible where the two types coincide.
* `wren_compiler.c:1053` — GCC 14 sees `parser.current` read on a path where it
  may not have been assigned.

Demoted to warnings for the `wren` component only, in
`components/wren/CMakeLists.txt`.

### 2. The compiler needs 33 KB of stack

The first attempt ran Wren on IDF's main task with the stack raised to 16 KB and
it **overflowed inside `wrenNewVM`** — before any user code, while compiling
Wren's own core library. Measured need is **33,456 B**, so it runs in a
dedicated task with 64 KB.

This is the finding that matters most for small parts: a CH32V006 has 8 KB of
RAM *in total*.

### 3. The default GC thresholds cannot work here, and failure is a crash

`wrenInitConfiguration` sets `initialHeapSize` to 10 MB and `minHeapSize` to
1 MB, and `vm->nextGC` starts at the former. With ~240 KB of heap free, **the
collector never runs**. Allocation eventually fails and Wren stores through the
returned pointer without checking it:

```
Guru Meditation Error: Core 0 panic'ed (Store access fault)
A0 : 0x00000000
```

It reads like a Wren bug and is a configuration one. This port sets 64 KB and
16 KB.

**Out of the box, upstream Wren crashes on this class of part.** Not slowly, not
with a diagnostic — a null store on the first benchmark that allocates.

### 4. Configuring the collector nearly halved the VM's resident size

| | VM resident |
|---|---|
| default thresholds | 143,972 B |
| 64 KB / 16 KB | **83,008 B** |

A 42% reduction, and it costs nothing: on the defaults the garbage from
compiling the core library is never collected, so it stays resident for the life
of the VM.

## What the Rust implementation has to beat

**83 KB resident and 33 KB of stack**, on a part that has 512 KB. Neither number
comes close to fitting a CH32V006's 8 KB, which is the strongest argument yet
for step 4 of the plan — compiling to bytecode on a host, so a constrained part
never carries the compiler that needs the 33 KB.

## A loose end

`fib` reports **216 bytes not returned** after `wrenFreeVM`. Small, repeatable,
and not yet chased: it may be IDF heap accounting rather than Wren. Worth
resolving before the leak figure is quoted anywhere.
