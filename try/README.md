# try/ — every binary this repository can build

Built by `tools/build-variants.sh`, which is the only thing that should
write here. Nothing in this folder is committed: it is a scratch tree, and
rebuilding it is how its numbers stay true.

Commit: `54919ce37ac8`

## ESP32-C6

`./flash.sh <variant>` writes one to the board. The ELF is what the
debugger reads; the **image** is what the partition has to hold, and is the
number to compare.

| variant | image | elf | profile | what it is |
|---|---|---|---|---|
| `bench-f32-speed` | 457,264 B | 791,388 B | speed | Singles instead of doubles. Faster and smaller, and not Wren: exact on integers only to 2^24. |
| `bench-f64-size` | 291,280 B | 705,328 B | size | The same at -Os, which is what a firmware with other things to do would use. |
| `bench-f64-speed` | 472,928 B | 806,904 B | speed | Full Wren, doubles, compiler on the device. The benchmark runner, and the build every published speed number comes from. |
| `bench-nofp-speed` | 400,512 B | 640,712 B | speed | Integers only. The fastest and smallest of the three, with no fractions at all. |
| `boot-compiler` | 237,152 B | 596,940 B | size | The same, with the compiler linked, which is what the compiler costs in flash. |
| `boot-default` | 195,216 B | 476,856 B | size | The smallest thing that starts a VM at all. |
| `uwren-binary_trees-size` | 171,728 B | 393,196 B | size | Tailored image for binary_trees: integers, no compiler, only the core its manifest asks for, frozen into flash. |
| `uwren-fib-size` | 171,728 B | 393,196 B | size | Tailored image for fib: integers, no compiler, only the core its manifest asks for, frozen into flash. |
| `uwren-fullcore-size` | 191,872 B | 456,060 B | size | The same program carrying the whole core library, so the tailoring can be priced. |
| `uwren-list_build-size` | 171,728 B | 393,396 B | size | Tailored image for list_build: integers, no compiler, only the core its manifest asks for, frozen into flash. |
| `uwren-method_call-size` | 171,728 B | 393,196 B | size | Tailored image for method_call: integers, no compiler, only the core its manifest asks for, frozen into flash. |
| `uwren-tailored-size` | 171,728 B | 392,624 B | size | The small one: integers, no compiler, only the core methods fib's manifest asks for, and that core frozen into the image. |
| `wrenc-f64-size` | 195,360 B | 462,208 B | size | Bytecode only, no compiler linked. What a device runs when a workstation did the parsing. |

## x86

`./run.sh <variant>` runs one. They read nothing from disk — the program
is bytecode compiled into the executable.

| variant | executable | what it is |
|---|---|---|
| `binary_trees-full` | 761,600 B | Full Wren as a host executable, with the program compiled in as bytecode. |
| `binary_trees-uwren` | 501,712 B | Tailored: integers, no compiler, only the core the program's manifest asks for, frozen into the image. |
| `fib-full` | 760,512 B | Full Wren as a host executable, with the program compiled in as bytecode. |
| `fib-uwren` | 501,264 B | Tailored: integers, no compiler, only the core the program's manifest asks for, frozen into the image. |
| `list_build-full` | 760,432 B | Full Wren as a host executable, with the program compiled in as bytecode. |
| `list_build-uwren` | 501,792 B | Tailored: integers, no compiler, only the core the program's manifest asks for, frozen into the image. |
| `method_call-full` | 761,424 B | Full Wren as a host executable, with the program compiled in as bytecode. |
| `method_call-uwren` | 502,528 B | Tailored: integers, no compiler, only the core the program's manifest asks for, frozen into the image. |

## Reading the table

**The tailored images are all the same size.** Four programs, four cores
generated from four different manifests — 5,746 to 6,355 bytes of generated
Rust between them — and one image size. The flash image is laid out in
aligned segments, and a few hundred bytes of difference disappears inside
the padding. The ELFs differ; the images do not.

**`-Os` against `-O3` is the largest single lever on the image**, larger
than any feature here: the same full-Wren build is 472,928 B at `speed` and
291,280 B at `size`. Every published speed number comes from the `speed`
build, and every image comparison elsewhere in the docs from `size`, so the
two are not interchangeable.

**What tailoring is worth** is `uwren-fullcore-size` against
`uwren-tailored-size`: the same program, the same profile, differing only
in whether the core is the whole library or the part its manifest asks for.

**What the compiler costs** is `boot-default` against `boot-compiler`:
the same firmware with and without the lexer and parser linked in.
