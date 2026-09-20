# A Wren program as a native executable

`tools/make_native_executable.sh` builds an executable with a Wren program
compiled into it. The program is carried as bytecode, not as text, so the
result reads nothing from disk and — in the tailored mode — has no compiler in
it at all.

```sh
tools/make_native_executable.sh benchmarks/wren/fib.wren            # full Wren
tools/make_native_executable.sh benchmarks/wren/fib.wren --uwren    # the small one
tools/make_native_executable.sh doc/examples --uwren --run          # a whole folder
```

## Why build one

The interesting failures in a tailored build do not look like failures. A
method symbol numbered differently, or a frozen method table indexed against
the wrong install order, reaches a device as a wrong answer and nothing else —
and a flash-and-watch cycle on the part is the better part of a minute.

The same program built for the host runs in milliseconds and fails in exactly
the same ways, because it is the same VM with the same features. So this is
where to iterate; the board is for confirming the result and measuring memory,
which is the one thing the host cannot tell you.

`cargo test --test frozen` is the narrower version of the same idea: six checks
on `fib` in about a tenth of a second.

## Choosing what goes in

**A file** builds one executable from that program.

**A directory** builds one executable per `.wren` directly inside it. Not a
recursive sweep: a tree of Wren usually has imports and fixtures in it that are
not programs, and building those produces confusing failures rather than useful
ones. Point it at the directory holding the programs you mean.

Each executable is named after its source, and lands in
`target/native/<name>/target/release/<name>`. `--out DIR` puts the build
somewhere else, which is worth using when comparing two modes — otherwise the
second build overwrites the first.

`--run` runs each one as it is built and indents the output.

## The two modes

**Default — full Wren.** The whole core library, `Num` as a double, and the
compiler linked in. This is Wren as specified, and what the conformance suite
runs against.

**`--uwren` — the tailored build.** Four things change together:

  - **Integers instead of doubles.** `Num` is an `i32`. Not Wren, and a program
    that counts past 2^30 will say so.
  - **No compiler.** The program was compiled when the executable was built.
  - **Only the core methods the program asks for.** The `.wrenc` carries a
    manifest listing every signature the code can call and every variable it
    can name; everything else in the core is not installed, and the classes a
    program cannot name are not built.
  - **That core frozen into the image.** The classes, their method tables and
    their names are generated as Rust and compiled in, rather than built in RAM
    at start-up.

It is the same combination `ports/esp32c6-wrenc-rs` ships, which is the point:
a program that works here works there.

### What that costs, on `fib`

| | full | `--uwren` |
|---|---|---|
| executable | 755,776 B | 499,016 B |
| bytecode compiled in | 589 B | 589 B |

Most of either figure is the Rust standard library, which a firmware does not
carry — see `doc/wren-rs/memory.md` for the numbers that matter on a part.

## How it is built

Four steps, in `target/native/<name>/`:

1. `examples/wrenc` compiles the `.wren` to `program.wrenc`.
2. For `--uwren`, `examples/freeze` reads that bytecode's manifest, builds the
   VM it implies, and writes the resulting core out as `frozen_core.rs`.
3. A small crate is generated around both, with a `main.rs` that loads the
   bundled bytecode and runs it.
4. `cargo build --release` with `lto` and `strip`.

Everything is generated, so deleting `target/native` costs nothing but time.

## The one way to get this wrong

**Generate a core with one feature set and compile it with another.**

A frozen method table holds *primitive indices*, and those are positions in the
sequence of `define` calls that `core::install` makes — which the cargo
features decide. A build without `core_full` makes fewer of them, so the same
signature ends up at a different index. Mix the two and every method entry
points at a plausible, wrong primitive.

Nothing about that looks like an error, and the symbol table does not move when
it happens: the manifest decides which *signatures* are interned either way, so
comparing symbols reports agreement. It has already happened once here, and
`fib` died claiming `Range` had no `iterate(_)`.

Two things stop it. `FrozenCore::primitives` records how many primitives
existed when the core was generated, and `disagreement` checks that first — the
generated `main.rs` calls it at start-up and refuses to run on a mismatch. And
the generated file records the features it was made with, in its header:

```
// Generated against this feature set, and only valid for a build with
// the same one -- the primitive indices below are positions in the
// sequence of `define` calls, which the features decide:
//     core_full  false
//     nofp       true
//     f32        false
```

The script generates the core with the features it is about to compile, so
using it is the way not to hit this. Hand-copying a generated core between
builds is the way to hit it.

## Related

  - `doc/wren-rs/memory.md` — what a frozen core saves, measured on an
    ESP32-C6, and what is left.
  - `crates/wren/src/frozen.rs` — the `FrozenCore` type and what still runs at
    start-up.
  - `crates/wren/examples/freeze.rs` — the generator.
  - `ports/esp32c6-wrenc-rs/src/bin/uwren.rs` — the same arrangement on a part,
    with a fixed heap and no allocator underneath.
