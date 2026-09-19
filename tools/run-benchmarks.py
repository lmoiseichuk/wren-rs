#!/usr/bin/env python3
"""Run Wren's own benchmark set on a board and record time, heap and stack.

    tools/run-benchmarks.py <by-id path> [repeats]

**Upstream's benchmark files, not ours.** `vendor/wren/test/benchmark/*.wren`
is the set the language's authors profile against -- `delta_blue` in particular,
which is what `doc/instruction counts.txt` is measured on, and which is the
long-standing cross-language workload inherited from Self and Smalltalk. Using
those rather than three hand-written loops means these numbers can be set beside
published ones instead of only beside each other.

Each file times itself with `System.clock` and prints `elapsed: <seconds>`. The
port adds what the file cannot see: wall time from the device's own timer, the
heap consumed, and the stack high-water mark.

`api_call` and `api_foreign_method` are skipped -- they need the C embedding
harness, which is out of scope here.

Results go to `doc/wren/benchmarks.json`.
"""

import json
import re
import statistics
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# **The scaled set, not upstream's.** Upstream's benchmarks are written for
# desktops and most of them do not fit: `for.wren` builds a million-element list
# (8 MB of values) and `binary_trees.wren` a depth-13 stretch tree (~768 KB),
# against 227 KB free. Running them unchanged reports "the VM crashed" where the
# truth is "this workload needs a workstation". See benchmarks/README.md.
SUITE = ROOT / "benchmarks" / "wren"

SKIP = set()

ELAPSED = re.compile(r"elapsed:\s*([0-9.eE+-]+)")
COST = re.compile(r"\[cost\]\s+(\d+) us\s+heap (\d+) B\s+stack (\d+) B")


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    port = sys.argv[1]
    repeats = int(sys.argv[2]) if len(sys.argv) > 2 else 3
    label = sys.argv[3] if len(sys.argv) > 3 else "unknown"

    # Reuse the suite runner's board handling: the reconnect-on-reboot and
    # resynchronise logic was all learned the hard way and there is no reason to
    # learn it twice.
    import importlib.util
    spec = importlib.util.spec_from_file_location(
        "runtests", Path(__file__).resolve().parent / "run-tests.py")
    runtests = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runtests)

    files = sorted(f for f in SUITE.glob("*.wren") if f.name not in SKIP)
    board = runtests.Board(port)

    print(f"{len(files)} benchmarks, {repeats} run(s) each\n")
    print(f"{'benchmark':<22} {'self (s)':>10} {'wall (ms)':>10} "
          f"{'heap (B)':>10} {'stack (B)':>10}")
    print("-" * 66)

    results = []
    for path in files:
        source = path.read_text(errors="replace")
        # A benchmark is allowed to be slow; delta_blue is the long one.
        timeout = 300.0 + len(source) / 4000.0

        selfs, walls, heaps, stacks, failures = [], [], [], [], 0
        for _ in range(repeats):
            got, status = board.run(source, timeout=timeout)
            if status != "ok":
                failures += 1
                continue
            text = "\n".join(got)
            match = ELAPSED.search(text)
            if match:
                selfs.append(float(match.group(1)))
            match = COST.search(text)
            if match:
                walls.append(int(match.group(1)) / 1000.0)
                heaps.append(int(match.group(2)))
                stacks.append(int(match.group(3)))

        def best(values):
            return statistics.median(values) if values else None

        row = {
            "benchmark": path.stem,
            "self_seconds": best(selfs),
            "wall_ms": best(walls),
            "heap_bytes": best(heaps),
            "stack_bytes": best(stacks),
            "runs": repeats,
            "failures": failures,
        }
        results.append(row)

        def show(value, fmt):
            return format(value, fmt) if value is not None else "-"

        note = f"  ({failures} failed)" if failures else ""
        print(f"{row['benchmark']:<22} {show(row['self_seconds'], '10.3f')} "
              f"{show(row['wall_ms'], '10.1f')} {show(row['heap_bytes'], '10,d')} "
              f"{show(row['stack_bytes'], '10,d')}{note}")

    # One file per build variant, so `-Os` and `-O2` can be set side by side
    # rather than one overwriting the other.
    out = ROOT / "doc" / "wren" / f"benchmarks-{label}.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({
        "meta": {
            "board": "ESP32-C6FH4 rev v0.2 @ 160 MHz",
            "port": "ports/esp32c6-wren",
            "wren": "vendor/wren 0.4.0, unmodified",
            "repeats": repeats,
            "variant": label,
            "benchmarks": "benchmarks/wren -- scaled for this part, see benchmarks/README.md",
            "run": time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime()),
            "note": "median of the runs; self_seconds is the benchmark's own "
                    "System.clock figure, wall_ms is the device's timer",
        },
        "results": results,
    }, indent=1))
    print(f"\nwritten: {out.relative_to(ROOT)}")
    if board.reconnects:
        print(f"the board rebooted {board.reconnects} time(s)")


if __name__ == "__main__":
    main()
