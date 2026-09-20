# A worked node, in Wren

Two programs that together look like a device rather than a benchmark:
[`boot.wren`](boot.wren) establishes who the node is and refuses a calibration
that would report nonsense, and [`main.wren`](main.wren) is the application it
hands over to — sample, filter, decide whether a reading is worth the radio,
and account for what it sent.

They exist to answer a question the benchmark set cannot. `fib` and
`binary_trees` say how fast the interpreter is; they say nothing about whether
the language is *pleasant to write a node in*, or what a realistic program
costs in flash and RAM once it has classes, closures, fibers, maps and string
interpolation in it. These do.

## The convention

The firmware looks for a program called `boot`, runs it, then looks for one
called `main` and runs that. A name that is not there is skipped rather than
treated as a fault. That is MicroPython's convention and the reasoning is
MicroPython's too: `boot` is the file allowed to decide the conditions `main`
runs under, so it has to have finished first.

**The two share no names.** Each is compiled as its own module, and Wren
reports a top-level name that is never defined as an error at the end of the
module — so a class declared in `boot` is not visible in `main`. That is a real
constraint of separate compilation, not a simplification of the demo, and it is
also what lets either file be replaced without rebuilding the other.

## What each one is for

**`boot.wren`** — 167 lines, three classes:

- `Identity` formats the node's name once at start-up rather than per message,
  zero-padded so `soil-0007` sorts before `soil-0012`.
- `Curve` turns millivolts into percent from measured breakpoints, clamped at
  both ends so a disconnected probe cannot read 140% moisture, and linear
  between them because a handful of field points does not justify a fit.
- `SelfTest` runs each check in its own `Fiber` and keeps going after a
  failure — a node that stops at the first fault tells you one thing per power
  cycle, which turns a ten-minute diagnosis into an afternoon.

**`main.wren`** — 192 lines, five classes:

- `Noise` is a Park–Miller generator, deterministic on purpose: this file
  exists to be measured, and a run that draws different numbers each time
  cannot be compared with the one before it.
- `Window` is a rolling median, not a mean, because a probe's failure mode is a
  spike and a mean carries one spike for the whole width of the window.
- `Deadband` decides whether a reading has moved enough to transmit, and
  returns a *reason* rather than a bool — a node whose traffic is all
  `keepalive` is a node whose deadband is too wide, and that only shows up if
  the reason is kept.
- `Frame` is what would go on the air, rounded to a tenth on the way out.
- `Node` is the loop and the accounting that makes a run comparable.

Between them they exercise classes and constructors, getters, closures stored
in lists, fibers with `try` and `error`, maps, list mutation, string
interpolation, and `Fiber.abort` as an error path — which is most of what a
device program actually uses.

## Running them

On the host, either file on its own — the second argument is the source, which
makes the runner check the bytecode's digest against it before running:

```sh
cargo run -p wren --release --example run-wrenc -- \
    doc/examples/boot.wrenc doc/examples/boot.wren
```

On hardware, both in one VM, which is what the boot convention means:

```sh
cd ports/esp32c6-wren-boot
cargo build --profile size                      # looks for boot.wrenc, main.wrenc
cargo build --profile size --features compiler  # looks for boot.wren,  main.wren
```

What that port measures — image size, compile time against load time, and heap
left — is in
[`ports/esp32c6-wren-boot/README.md`](../../ports/esp32c6-wren-boot/README.md).
It is the clearest statement in the repository of what shipping bytecode
instead of source is worth.

## The `.wrenc` beside each `.wren`

The compiled form is committed so that a firmware build needs no host compiler,
and it is a build artefact that looks like a source file — which is exactly the
hazard, because a stale one looks current. Every `.wrenc` carries a SHA-256 of
the source it came from:

```sh
tools/build-bytecode.sh           # rebuild them
tools/build-bytecode.sh --check   # verify they match, build nothing
```

Run the check in CI and drift is caught by the build rather than by a device
behaving like a version of the program nobody can find.

## Also used as evidence

These two are part of the source population the heap profiler walks, alongside
upstream's test suite and the benchmarks, because they are the only programs
here shaped like something somebody would deploy. The conclusions that
population supports are in [`../wren-rs/memory.md`](../wren-rs/memory.md).
