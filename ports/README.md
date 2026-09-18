# Ports

One directory per **(board, implementation)** pair, named `<board>-<what>`. The
board comes first because that is what the three share: the whole point of this
project is comparing implementations on identical silicon, and a layout that
grouped by implementation would hide that.

| port | what it is | step |
|---|---|---|
| [`esp32c6-wren`](esp32c6-wren) | upstream Wren in C, unmodified, on the C6 | 1 — the reference |
| [`esp32c6-micropython`](esp32c6-micropython) | MicroPython on the same board | 2 — the baseline |
| [`esp32c6-wren-rs`](esp32c6-wren-rs) | this repository's Rust VM | 3 — the deliverable |

## The rule that makes the comparison mean anything

**The same board, the same clock, the same benchmark source, the same
measurement method.** A port that changes any of those produces a number that
cannot be put in the same table as the others. Where a port *must* differ — a
different console driver, a different heap size — it says so in its own README,
under a heading called "How this differs", so the caveat travels with the
number.

## What each port has to provide

Three things, so a benchmark run is the same command everywhere:

* **A console on the TTY.** Read a line, evaluate it, print the result.
* **A way to run a named benchmark** from the shared set, so the host does not
  have to paste source in over serial and time the echo.
* **A report of its own size and memory**: image size, static RAM, and free heap
  after start-up. Size is as much of the comparison as speed, and it is the part
  most easily left unmeasured because nothing fails when it grows.

## Upstream stays upstream

`vendor/wren` is a git submodule pinned to wren-lang/wren, and **nothing in this
repository modifies it**. A port that needs a change makes it in the port's own
files — a shim, a config header, a wrapper — never in the submodule. If a change
there turns out to be unavoidable, that is a finding worth writing down rather
than a patch worth hiding, because it means Wren as published does not build for
this class of part.
