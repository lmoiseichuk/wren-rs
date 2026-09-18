# esp32c6-wren-rs — the deliverable

This repository's Rust VM on the ESP32-C6.

**The point of the project.** A firmware that pulls in the `wren` crate the way
it pulls in any other, with no C build step, no submodule and no `build.rs`
compiling somebody else's tree.

## Status

Not built yet — the VM it depends on is still being written. See the root README
for what exists.

## What it will be

* A binary crate for `riscv32imac-esp-espidf`, depending on `wren-rs` with
  `default-features = false`.
* The same console, the same benchmark runner, and the same size and memory
  report as the other two ports, so the three tables line up.

## The measurement that matters most

**Free heap after start-up**, on a part where the whole budget is the point.
Speed is easier to argue about and easier to improve later; a VM that does not
leave the user program enough room is simply unusable, and that is the number
that decides whether the CH32-class targets are reachable at all.
