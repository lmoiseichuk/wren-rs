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
cargo build --profile size --features wren/core_full           # looks for boot.wrenc, main.wrenc
cargo build --profile size --features wren/core_full,compiler  # looks for boot.wren,  main.wren
cargo build --profile size --features wren/core_full,f32       # ...and with 32-bit numbers
```

**`wren/core_full` has to be asked for.** The manifest takes `wren` with
`default-features = false` and `alloc` and `libm`, and since d4299bf
(2026-09-20) the whole core library is the `core_full` feature, which that
leaves out. Built without it, `boot` runs and `main` stops at line 133 with
`Map metaclass does not implement 'new()'`, from an image 42-115 KB smaller
with half the VM resident -- 11,332 B against 23,176. Every figure below is
built with it.

Separate packages would have let the two drift apart in a dozen small ways that
each look like nothing and together make the comparison meaningless. Same heap,
same VM, same start-up, same reporting; the compiler is linked or it is not.

Both report the same three numbers per program, and the split between the first
two is the whole argument:

- **prepare** — compiling the source, or loading the bytecode.
- **run** — executing it, which should come out the same either way.
- **heap** — what the program left resident.

## What it costs and what it saves

ESP32-C6FH4 at 160 MHz, 320 KB heap, both programs run in one VM. Measured on
2026-10-13 at cac0550 on `c6b`: each image flashed twice with `espflash flash
--monitor`, as `tools/measure-rs.sh` does, and run to `done`. The two runs
agree on prepare and run to 2 µs; reset to idle varies by up to 1 ms, so both
runs are given. An image's size is `espflash save-image --chip esp32c6`'s,
which is what espflash reports as the app's size when it flashes.

| `-Os` | bytecode | compiler |
|---|---|---|
| image | **258,208 B** | 292,240 B |
| programs in image | 9,045 B | 11,815 B |
| prepare, `boot` | **4,512 µs** | 27,273 µs |
| prepare, `main` | **4,864 µs** | 32,364 µs |
| prepare, both files | **9,376 µs** | 59,637 µs |
| run, both files | **914,570 µs** | 963,937 µs |
| reset to idle | **940,638 and 940,951 µs** | 1,040,280 and 1,040,408 µs |
| heap left | **195,344 B** | 193,480 B |
| VM resident | 23,176 B | 23,176 B |

The same for the other three builds:

| | prepare, both files | run, both files | heap left | VM resident |
|---|---|---|---|---|
| `-O3` `f64` bytecode | 6,336 µs | 685,421 µs | 195,344 B | 23,176 B |
| `-O3` `f64` compiler | 53,393 µs | 619,950 µs | 193,480 B | 23,176 B |
| `-Os` `f32` bytecode | 9,139 µs | 854,272 µs | 218,912 B | 22,888 B |
| `-Os` `f32` compiler | 62,116 µs | 772,538 µs | 217,264 B | 22,888 B |
| `-O3` `f32` bytecode | 6,131 µs | 576,850 µs | 218,912 B | 22,888 B |
| `-O3` `f32` compiler | 56,921 µs | 561,285 µs | 217,264 B | 22,888 B |

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

**Preparation is 6.4x faster, and it does not matter much here.** Loading both
files takes 9.4 ms against 59.6 ms to compile them — a real difference, and an
irrelevant one against programs that then run for 0.9 s. It is worth having
when start-up latency is the thing being paid for: a node that wakes, samples
and sleeps pays the prepare cost on every wake and the run cost for a few
milliseconds. For that duty cycle the ratio inverts and this is the dominant
number.

**Running should be a wash, and is not.** Both builds execute the same program
through the same interpreter -- `main` reports the same frames from the same
2,000 cycles in both builds of a configuration, 107 in `f64` and 106 in `f32`
-- yet the compiler build's run is 5.4% longer at `-Os` in `f64`, and shorter
in the other three: by 9.6% at `-O3` in `f64`, 9.6% at `-Os` in `f32` and 2.7%
at `-O3` in `f32`. A difference whose sign changes with the build is what code
placement produces, and padding branch targets moved `binary_trees` by 1.7% and
`fib` by 1.4% (see the top-level README); but this port has no padding control
and no instruction counter, so that it is placement here is not shown, and
three of the four gaps are larger than any placement effect measured elsewhere,
3 to 4% at 80 MHz included.

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
