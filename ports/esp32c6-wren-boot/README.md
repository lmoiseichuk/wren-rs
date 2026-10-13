# A node that looks for `boot` and `main`

What a Wren device would actually be, rather than a benchmark harness: at
start-up it looks for a program called `boot`, runs it, then looks for one
called `main` and runs that. A name that is not there is skipped, not an error.

The convention is MicroPython's, and so is the reasoning — `boot` is the file
allowed to decide the conditions `main` runs under, so it has to have finished
first.

## The two builds

One package, one `main.rs`, two images. The only difference is a cargo feature:

```sh
cargo build --profile size                      # looks for boot.wrenc, main.wrenc
cargo build --profile size --features compiler  # looks for boot.wren,  main.wren
cargo build --profile size --features f32       # ...and with 32-bit numbers
```

The port lost the core library on 2026-09-20, when d4299bf made it the
`core_full` feature and the manifest did not name it, and got it back on
2026-10-13; in between, `main` stopped at its first `Map`.

Separate packages would have let the two drift apart in a dozen small ways that
each look like nothing and together make the comparison meaningless. Same heap,
same VM, same start-up, same reporting; the compiler is linked or it is not.

Both report the same three numbers per program, and the split between the first
two is the whole argument:

- **prepare** — compiling the source, or loading the bytecode.
- **run** — executing it: the same program either way, so what moves it
  between images is where the linker put the code.
- **heap** — what the program left resident.

## What it costs and what it saves

ESP32-C6FH4 at 160 MHz, 320 KB heap, both programs run in one VM. Measured on
2026-10-13 on `c6b`, from images this manifest builds with the commands above:
each image flashed twice with `espflash flash --monitor`, as
`tools/measure-rs.sh` does, and run to `done`. The two runs agree on prepare
and run to 1 µs; reset to idle varies by up to 0.5 ms, so both runs are given.
An image's size is `espflash save-image --chip esp32c6`'s, which is what
espflash reports as the app's size when it flashes.

| `-Os` | bytecode | compiler |
|---|---|---|
| image | **258,208 B** | 292,240 B |
| programs in image | 9,045 B | 11,815 B |
| prepare, `boot` | **4,506 µs** | 29,119 µs |
| prepare, `main` | **4,875 µs** | 33,389 µs |
| prepare, both files | **9,381 µs** | 62,508 µs |
| run, both files | 1,002,238 µs | 911,053 µs |
| reset to idle | 1,028,717 and 1,028,555 µs | 990,858 and 990,990 µs |
| heap left | **195,344 B** | 193,480 B |
| VM resident | 23,176 B | 23,176 B |

The same for the other three builds:

| | prepare, both files | run, both files | heap left | VM resident |
|---|---|---|---|---|
| `-O3` `f64` bytecode | 6,400 µs | 645,653 µs | 195,344 B | 23,176 B |
| `-O3` `f64` compiler | 53,806 µs | 690,744 µs | 193,480 B | 23,176 B |
| `-Os` `f32` bytecode | 9,145 µs | 854,237 µs | 218,912 B | 22,888 B |
| `-Os` `f32` compiler | 61,203 µs | 829,581 µs | 217,264 B | 22,888 B |
| `-O3` `f32` bytecode | 6,152 µs | 576,582 µs | 218,912 B | 22,888 B |
| `-O3` `f32` compiler | 57,014 µs | 561,245 µs | 217,264 B | 22,888 B |

> These figures used to be 80 MHz runs halved, as the rest of this repository's
> pre-2026-09-29 times are. Halving held for running -- the clock fix measured
> every benchmark at 50.0-50.2% of its 80 MHz time
> ([`doc/wren-rs/profiling.md`](../../doc/wren-rs/profiling.md), *The clock*)
> -- and not for preparing: c8b4b34 at 80 MHz, run again on `c6b`, prepared
> both files in 9,851 µs and 63,166 µs, and c75f9f5, the commit that set 160
> MHz, built with `wren/core_full`, in 8,395 and 60,458 µs, where halving
> predicted 4,911 and 32,077. The two commits differ in code as well as clock,
> and why a one-off pass barely speeds up with the clock has not been tested.

Images for all four builds, which is the whole of what the two features are
worth on flash:

| | `-Os` | `-O3` |
|---|---|---|
| `f64` bytecode | 258,208 B | 399,648 B |
| `f64` + compiler | 292,240 B | 476,768 B |
| `f32` bytecode | 241,776 B | 382,912 B |
| `f32` + compiler | 276,016 B | 459,984 B |

Read across the rows rather than down the columns:

**The image is where bytecode wins.** 34,032 B at `-Os` and 77,120 B at `-O3`.
Of the `-Os` saving, 2,770 B is the programs themselves being smaller as
bytecode than as source; the rest, 31,262 B, is the lexer and the parser. It
barely moves with the number width — 34,032 B against 34,240 B — because a
lexer does not care how wide a double is. That is why the compiler is quoted
as a flat cost rather than folded into a runtime comparison.

**`f32` is worth 16,432 B at `-Os`** on top of whichever of those you pick, and
it is a runtime saving rather than a static one: it also leaves 23,568 B more
heap. It is off by default and it is not Wren — see `crates/wren`'s own feature
documentation.

**Preparation is 6.7x faster, and it does not matter much here.** Loading both
files takes 9.4 ms against 62.5 ms to compile them — a real difference, and an
irrelevant one against programs that then run for 0.9 to 1.0 s. It is worth
having when start-up latency is the thing being paid for: a node that wakes,
samples and sleeps pays the prepare cost on every wake and the run cost for a
few milliseconds. For that duty cycle the ratio inverts and this is the dominant
number.

**The run gap between two images is code placement, not bytecode against
compiler.** Both builds execute the same program through the same interpreter
-- `main` reports the same frames from the same 2,000 cycles in both builds of
a configuration, 107 in `f64` and 106 in `f32` -- and the same source and
features, built earlier the same day in another directory and run on `c6b`,
show what moves the time. Those eight images had the same sizes, heap left and
VM resident as these, and not the same bytes: a panic message carries its
source file's path, so `.rodata` changes size -- by 464 B in the `-Os` `f64`
bytecode image -- and the functions move with it. Prepare here is within 4.8%
of what those images took and run is up to 11.4% from it, and in `f64` the gap
between bytecode and compiler changed sign: the compiler image ran 5.4% longer
than the bytecode one at `-Os` there and runs 9.1% shorter here, and at `-O3`
ran 9.6% shorter there and runs 7.0% longer here. In `f32` it kept its sign -- 9.6% shorter there and 2.9% here at `-Os`, 2.7% at `-O3` in both.
A gap whose sign the checkout's path decides says nothing about the images,
and at up to 9.6% it is larger than any placement effect measured on the
benchmarks, 3 to 4% at 80 MHz included (see the top-level README). **Only
prepare -- loading against compiling -- is a property of the images**: 6.4x at
`-Os` in `f64` there and 6.7x here, where run went from 5.4% one way to 9.1%
the other.

**1,864 B less heap resident**, because the compiler's working structures are
not allocated and the source strings are not interned.

## The one thing bytecode costs

A build with no compiler cannot compile anything, ever. No REPL, no
`eval`, no loading a program that arrives over the air as text. Whatever such a
node runs has to have been compiled on a machine that had a compiler. That is
the trade, and the 31 KB is what it is worth.

## The programs

`doc/examples/boot.wren` sets up: identity, the calibration curve, and a power-on
self-test that refuses a curve which would report nonsense.
`doc/examples/main.wren` is the application: sample, median-filter, decide against a
deadband whether the reading is worth the radio, and account for what it sent.

**They share no names.** Wren rejects a top-level name that is never defined, so
a class declared in `boot` is not visible in `main` — separate compilation means
separately compiled. The seam is along a real join: calibration is boot's
concern, the schedule and the radio are main's.

Bytecode is committed beside the source because the ports `include_bytes!` it
and a firmware build should not need a host compiler. It carries a SHA-256 of
the source it came from, so drift is checkable rather than invisible:

```sh
tools/build-bytecode.sh           # rebuild
tools/build-bytecode.sh --check   # verify, build nothing
```
