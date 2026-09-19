# esp32c6-wren — the reference

Upstream Wren 0.4.0, in C, unmodified, running on an ESP32-C6 over USB
Serial/JTAG.

**This is the control, not a product.** It exists so that every later claim
about the Rust implementation is a difference measured against the same
language on the same silicon.

## What it needs

ESP-IDF v5.5 with the `esp32c6` target, and `python3-serial` for the host
tools. Nothing else — Wren has no dependencies of its own, which is part of why
it was a reasonable thing to port.

```sh
# Prerequisites
sudo apt update
sudo apt install -y git wget flex bison gperf python3 python3-pip python3-venv \
                    cmake ninja-build ccache libffi-dev libssl-dev dfu-util \
                    libusb-1.0-0 python3-serial

# Serial access, then log out and back in
sudo usermod -aG dialout "$USER"

# ESP-IDF v5.5 with the C6 target
mkdir -p ~/.espressif/esp-idf && cd ~/.espressif/esp-idf
git clone -b v5.5 --recursive https://github.com/espressif/esp-idf.git v5.5
cd v5.5 && ./install.sh esp32c6
```

Two that are not in Espressif's instructions and cost time here:

* **`python3-serial`** — not on their list, but every tool in `tools/` needs it.
* **Run the whole `install.sh`.** A tree missing any one tool refuses to export,
  even a tool a build never uses. This bench had everything except
  `openocd-esp32` — a debugger — and `export.sh` failed with
  `Activation script failed` until it was fetched:

  ```sh
  ~/.espressif/python_env/idf5.5_py3.13_env/bin/python \
      ~/.espressif/esp-idf/v5.5/tools/idf_tools.py install openocd-esp32
  ```

If IDF lives elsewhere, `export IDF_EXPORT=/path/to/export.sh`.

Two things specific to this port:

* **`vendor/wren` must be checked out.** It is a submodule and the CMake here
  globs its `src/vm` and `src/optional` directly; an un-initialised submodule
  fails as a missing source directory rather than as a missing submodule.

  ```sh
  git submodule update --init --recursive
  ```
* **The `tests` variant also compiles `vendor/wren/test/api/*.c`**, upstream's
  foreign-function fixtures. Those are test code, so `size` and `perf` leave
  them out — see the variants below.

## Builds

Three, each stored under `release/` with the `esptool.py` line to flash it with
no toolchain at all:

| variant | flags | image | for |
|---|---|---|---|
| `size` | `-Os` | 272,736 B | the published footprint, and the comparison against MicroPython |
| `perf` | `-O2` | 301,888 B | the speed ceiling — `-O2` costs **+29 KB** |
| `tests` | `-Os` + api fixtures | 287,008 B | the test suite — the fixtures cost **+14 KB** |

```sh
tools/flash.sh esp32c6-wren size        # build and flash one
tools/release.sh esp32c6-wren perf      # build and store it under release/
```

**One build directory per variant, each with its own `sdkconfig`.** That is not
tidiness: ESP-IDF regenerates `sdkconfig` from `sdkconfig.defaults` only when
there is no `sdkconfig` yet, so a stale one silently ignores every later change
to the defaults. A whole test run happened here with the task watchdog still
enabled after it had been switched off, producing seven reboots and sixteen
timeouts that were read as Wren failures.

## Status: running

```
tools/flash.sh esp32c6-wren            # build and flash
tools/flash.sh esp32c6-wren --monitor  # and stay attached
```

Console commands: `.bench`, `.mem`, `.stack`, `.help`. Anything else is
evaluated as Wren.

## Results — 2026-09-18, ESP32-C6FH4 rev v0.2 @ 160 MHz, IDF v5.5, `-Os`

### Footprint

| | |
|---|---|
| image | **274 KB** (`0x43120`) |
| heap free at boot | 385,684 B |
| **VM resident** | **83,008 B** |
| **compiler stack** | **33,456 B** |

### Benchmarks

| benchmark | time | peak heap | what it exercises |
|---|---|---|---|
| `fib` | **752 ms** | 90,048 B | method dispatch, arithmetic |
| `tree` | **1,898 ms** | 180,116 B | allocation and the collector |
| `loop` | **1,901 ms** | 86,420 B | interpreter dispatch, soft float |

`fib` is `Fib.of(24)`; `tree` builds a depth-10 tree forty times; `loop` is
200,000 iterations of `x = x + i % 7`. Source is in `main/main.c` so it travels
with the numbers.

## How this differs

Four places where Wren as published did not work here. **None is a change to the
submodule** — the first is a build flag, the rest are configuration or port
code. Each is a requirement the Rust implementation inherits.

### 1. It does not compile under IDF's warnings

IDF builds with `-Werror=all -Wextra`. Two become errors:

* `wren_compiler.c:4127` — `%.*s` takes its precision as `int`; the argument is
  `int32_t`, which is `long int` on this target. A genuine portability bug that
  is invisible where the two types coincide.
* `wren_compiler.c:1053` — GCC 14 sees `parser.current` read on a path where it
  may not have been assigned.

Demoted to warnings for the `wren` component only, in
`components/wren/CMakeLists.txt`.

### 2. The compiler needs 33 KB of stack

The first attempt ran Wren on IDF's main task with the stack raised to 16 KB and
it **overflowed inside `wrenNewVM`** — before any user code, while compiling
Wren's own core library. Measured need is **33,456 B**, so it runs in a
dedicated task with 64 KB.

This is the finding that matters most for small parts: a CH32V006 has 8 KB of
RAM *in total*.

### 3. The default GC thresholds cannot work here, and failure is a crash

`wrenInitConfiguration` sets `initialHeapSize` to 10 MB and `minHeapSize` to
1 MB, and `vm->nextGC` starts at the former. With ~240 KB of heap free, **the
collector never runs**. Allocation eventually fails and Wren stores through the
returned pointer without checking it:

```
Guru Meditation Error: Core 0 panic'ed (Store access fault)
A0 : 0x00000000
```

It reads like a Wren bug and is a configuration one. This port sets 64 KB and
16 KB.

**Out of the box, upstream Wren crashes on this class of part.** Not slowly, not
with a diagnostic — a null store on the first benchmark that allocates.

### 4. Configuring the collector nearly halved the VM's resident size

| | VM resident |
|---|---|
| default thresholds | 143,972 B |
| 64 KB / 16 KB | **83,008 B** |

A 42% reduction, and it costs nothing: on the defaults the garbage from
compiling the core library is never collected, so it stays resident for the life
of the VM.

### 5. A program that computes for more than a few seconds reboots the board

Wren's interpreter loop runs to completion. It does not block, does not yield,
and offers no hook to do either — so on FreeRTOS the idle task is starved and
the task watchdog fires:

```
E (32922) task_wdt: Task watchdog got triggered.
E (32922) task_wdt:  - IDLE (CPU 0)
```

**Seven of upstream's twelve benchmarks died this way**, including `delta_blue`,
`binary_trees` and `fib`. Every one of them reads as "Wren crashed" until the
watchdog line is spotted in the output.

This port disables the watchdog, which is right for a benchmark rig and wrong
for a product. **A VM meant to run alongside anything else needs a yield hook**,
and Wren has none — that is a requirement the Rust implementation should meet
rather than inherit.

## What the Rust implementation has to beat

**83 KB resident and 33 KB of stack**, on a part that has 512 KB. Neither number
comes close to fitting a CH32V006's 8 KB, which is the strongest argument yet
for step 4 of the plan — compiling to bytecode on a host, so a constrained part
never carries the compiler that needs the 33 KB.

## A loose end

`fib` reports **216 bytes not returned** after `wrenFreeVM`. Small, repeatable,
and not yet chased: it may be IDF heap accounting rather than Wren. Worth
resolving before the leak figure is quoted anywhere.
