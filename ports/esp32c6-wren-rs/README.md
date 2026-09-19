# esp32c6-wren-rs — the deliverable

This repository's Rust VM on the ESP32-C6.

**The point of the project.** A firmware that pulls in the `wren` crate the way
it pulls in any other, with no C build step, no submodule and no `build.rs`
compiling somebody else's tree.

## What it needs

A Rust toolchain with the ESP-IDF target, plus the same ESP-IDF this project's
C port uses — `esp-idf-sys` drives the IDF build underneath Cargo.

```sh
# Rust, if it is not already here
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# The RISC-V ESP-IDF target and the build helper
rustup target add riscv32imac-esp-espidf
cargo install ldproxy espflash

# Serial access, then log out and back in
sudo usermod -aG dialout "$USER"

# ESP-IDF prerequisites, which esp-idf-sys needs present
sudo apt update
sudo apt install -y git wget flex bison gperf python3 python3-pip python3-venv \
                    cmake ninja-build ccache libffi-dev libssl-dev dfu-util \
                    libusb-1.0-0 python3-serial
```

**No `xtensa` toolchain and no `espup`.** The C6 is RISC-V, so the stock Rust
target is enough — that whole layer only appears for Xtensa parts.

What it does *not* need is the thing worth pointing at: **no C compiler for the
VM, no submodule, no `build.rs` compiling somebody else's tree.** The
`wren` crate is pure Rust with no dependencies. That is the deliverable, and the
contrast with the C port's setup above is a large part of the point.

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
