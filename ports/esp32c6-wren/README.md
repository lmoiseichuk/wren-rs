# esp32c6-wren — the reference

Upstream Wren 0.4.0, in C, running on the ESP32-C6, talking over the TTY.

**This is the control, not a product.** Nothing here is meant to be shipped; it
exists so that every later claim about the Rust implementation is a difference
measured against the same language on the same silicon, rather than against a
remembered number from a different machine.

It also answers a question worth having early, before any Rust is written:
**does Wren as published even fit and run on this part?** If upstream needs
concessions to work here, those concessions are the real specification for the
Rust version.

## Status

Not built yet.

## What it needs

* ESP-IDF project embedding `vendor/wren/src/vm` and `src/optional`.
* A `WrenConfiguration` whose `writeFn` and `errorFn` go to the console.
* Heap: Wren's allocator hook wired to the IDF heap, so the numbers it reports
  are the numbers the part actually has.
* The three things `ports/README.md` asks of every port: a console, a benchmark
  runner, and a size and memory report.

## How this differs

Nothing yet. Anything that ends up here is a place where upstream could not be
used as published — and each entry is a requirement the Rust implementation
inherits.
