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

Separate packages would have let the two drift apart in a dozen small ways that
each look like nothing and together make the comparison meaningless. Same heap,
same VM, same start-up, same reporting; the compiler is linked or it is not.

Both report the same three numbers per program, and the split between the first
two is the whole argument:

- **prepare** — compiling the source, or loading the bytecode.
- **run** — executing it, which should come out the same either way.
- **heap** — what the program left resident.

## What it costs and what it saves

ESP32-C6FH4 at 160 MHz, 320 KB heap, both programs run in one VM.

| `-Os` | bytecode | compiler |
|---|---|---|
| image | **244,960 B** | 279,440 B |
| programs in image | 8,422 B | 11,815 B |
| prepare, both files | **9,821 µs** | 64,154 µs |
| run, both files | 3,196,086 µs | 3,211,906 µs |
| reset to idle | **3,228,083 µs** | 3,298,642 µs |
| heap left | **183,040 B** | 172,552 B |
| VM resident | 24,680 B | 24,680 B |

Images for the other three builds, which is the whole of what the two features
are worth on flash:

| | `-Os` | `-O3` |
|---|---|---|
| `f64` bytecode | 244,960 B | 363,568 B |
| `f64` + compiler | 279,440 B | 444,784 B |
| `f32` bytecode | 227,712 B | 343,776 B |
| `f32` + compiler | 262,256 B | 425,104 B |

Read across the rows rather than down the columns:

**The image is where bytecode wins.** 34,480 B at `-Os` and 81,216 B at `-O3`.
Of the `-Os` saving, 3,393 B is the programs themselves being smaller as
bytecode than as source; the rest is the lexer and the parser. It barely moves
with the number width — 34,480 B against 34,544 B — because a lexer does not
care how wide a double is. That is why the compiler is quoted as a flat cost
rather than folded into a runtime comparison.

**`f32` is worth 17,248 B at `-Os`** on top of whichever of those you pick, and
it is a runtime saving rather than a static one: it also takes a fifth to a
half off the heap. It is off by default and it is not Wren — see
`crates/wren`'s own feature documentation.

**Preparation is 6.5x faster, and it does not matter much here.** Loading both
files takes 9.8 ms against 64.2 ms to compile them — a real difference, and an
irrelevant one against programs that then run for three seconds. It is worth
having when start-up latency is the thing being paid for: a node that wakes,
samples and sleeps pays the prepare cost on every wake and the run cost for a
few milliseconds. For that duty cycle the ratio inverts and this is the
dominant number.

**Running is a wash, as it should be.** Both builds execute the same bytecode
through the same interpreter, and the half-percent between them is
instruction-cache layout. That the two agree is the evidence the comparison is
sound — a real difference here would mean the two builds were not running the
same program.

**10,488 B less heap resident**, because the compiler's working structures are
not allocated and the source strings are not interned.

## The one thing bytecode costs

A build with no compiler cannot compile anything, ever. No REPL, no
`eval`, no loading a program that arrives over the air as text. Whatever such a
node runs has to have been compiled on a machine that had a compiler. That is
the trade, and the 31 KB is what it is worth.

## The programs

`programs/boot.wren` sets up: identity, the calibration curve, and a power-on
self-test that refuses a curve which would report nonsense.
`programs/main.wren` is the application: sample, median-filter, decide against a
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
