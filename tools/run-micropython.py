#!/usr/bin/env python3
"""Run `benchmarks/python/*.py` on a MicroPython board and record what they cost.

    tools/run-micropython.py <by-id path> [repeats]

**The counterpart to run-benchmarks.py, measuring the same things the same way.**
A comparison is only worth printing if both sides were put on the scales
identically, and two things had to be matched deliberately:

*Heap.* The Wren port reports `free_heap()` before a run minus `free_heap()`
after it -- **no forced collection**, so the figure is "heap consumed and not
yet reclaimed when the run ended". The obvious MicroPython analogue,
`gc.mem_free()` before and after, is the same measurement, so that is what is
reported as `heap_bytes`. A `gc.collect()` afterwards gives a second, different
number -- the genuinely live set -- and that is reported separately as
`live_bytes` rather than conflated with the first. An earlier pass measured
MicroPython after a collection and Wren before one, and produced numbers like
"192 B" against Wren's "134,712 B" for the same program, which is not a 700x
result, it is two different questions.

*State between runs.* Each run gets a soft reset first, so a global left behind
by the previous benchmark -- `list_build` keeps a 10,000-element list alive at
module scope -- cannot be counted against the next one's baseline.

The program's own `elapsed:` line is its `time.ticks_us` figure, matching Wren's
`System.clock`; both exclude the harness.
"""

import json
import re
import statistics
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SUITE = ROOT / "benchmarks" / "python"
BAUD = 115200

ELAPSED = re.compile(r"elapsed:\s*([0-9.eE+-]+)")
COST = re.compile(r"\[cost\]\s+heap (-?\d+) B\s+live (-?\d+) B")

# The benchmark runs through `exec(..., globals())` so its top-level names land
# in the module namespace, the way a Wren script's do -- a plain `exec` inside a
# function would make them locals and free them on return, which is exactly the
# mistake that produced the bogus 192 B.
WRAPPER = """import gc
_src = {source!r}
gc.collect()
_before = gc.mem_free()
exec(_src, globals())
_after = gc.mem_free()
gc.collect()
_live = gc.mem_free()
print("[cost] heap %d B  live %d B" % (_before - _after, _before - _live))
"""


class Board:
    def __init__(self, port):
        import serial
        import fcntl
        self.link = serial.Serial(port, BAUD, timeout=0.5)
        try:
            fcntl.flock(self.link.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            raise SystemExit(f"{port} is held by another tool")
        time.sleep(1.5)
        self.link.write(b"\x03\x03")
        time.sleep(0.2)
        self.link.reset_input_buffer()

    def soft_reset(self):
        """Ctrl-B to the friendly REPL, Ctrl-D to soft reset, then wait."""
        self.link.write(b"\x03\x03")
        time.sleep(0.2)
        self.link.write(b"\x02")          # friendly REPL
        time.sleep(0.3)
        self.link.reset_input_buffer()
        self.link.write(b"\x04")          # soft reset
        deadline = time.monotonic() + 10
        seen = b""
        while time.monotonic() < deadline:
            seen += self.link.read(256)
            if b">>>" in seen:
                break
        time.sleep(0.3)
        self.link.reset_input_buffer()

    def run(self, source, timeout):
        self.link.write(b"\x01")          # raw REPL
        time.sleep(0.3)
        banner = self.link.read(256).decode("utf-8", "replace")
        if "raw REPL" not in banner:
            print(f"  warning: no raw REPL banner ({banner.strip()[:60]!r})",
                  file=sys.stderr)

        payload = source.encode("utf-8")
        for at in range(0, len(payload), 128):
            self.link.write(payload[at:at + 128])
            self.link.flush()
            time.sleep(0.02)
        self.link.write(b"\x04")
        self.link.flush()

        collected = b""
        started = time.monotonic()
        while time.monotonic() - started < timeout:
            chunk = self.link.read(1024)
            if chunk:
                collected += chunk
                started = time.monotonic()
                if collected.count(b"\x04") >= 2:
                    break
        text = collected.decode("utf-8", "replace")
        # raw REPL answers `OK`, output, \x04, traceback, \x04
        body, _, rest = text.partition("\x04")
        trace = rest.partition("\x04")[0]
        return body.replace("OK", "", 1).strip(), trace.strip()

    def query(self, expression):
        self.soft_reset()
        out, trace = self.run(f"print({expression})", timeout=10)
        return trace if trace else out


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    port = sys.argv[1]
    repeats = int(sys.argv[2]) if len(sys.argv) > 2 else 3

    board = Board(port)

    version = board.query("__import__('sys').implementation")
    free = board.query("(__import__('gc').collect(), __import__('gc').mem_free())")
    alloc = board.query("(__import__('gc').collect(), __import__('gc').mem_alloc())")
    print(f"implementation : {version}")
    print(f"free at boot   : {free}")
    print(f"alloc at boot  : {alloc}\n")

    files = sorted(SUITE.glob("*.py"))
    print(f"{len(files)} benchmarks, {repeats} run(s) each\n")
    print(f"{'benchmark':<22} {'self (s)':>10} {'spread':>9} "
          f"{'heap (B)':>10} {'live (B)':>10}")
    print("-" * 66)

    results = []
    for path in files:
        source = path.read_text(errors="replace")
        wrapped = WRAPPER.format(source=source)

        selfs, heaps, lives, failures, traces = [], [], [], 0, []
        for _ in range(repeats):
            board.soft_reset()
            body, trace = board.run(wrapped, timeout=300)
            if trace:
                failures += 1
                traces.append(trace.splitlines()[-1] if trace else "")
                continue
            match = ELAPSED.search(body)
            if match:
                selfs.append(float(match.group(1)))
            match = COST.search(body)
            if match:
                heaps.append(int(match.group(1)))
                lives.append(int(match.group(2)))

        def best(values):
            return statistics.median(values) if values else None

        # The spread across runs, which is the thing a single-run figure hides.
        spread = None
        if len(selfs) > 1 and best(selfs):
            spread = (max(selfs) - min(selfs)) / best(selfs) * 100.0

        row = {
            "benchmark": path.stem,
            "self_seconds": best(selfs),
            "self_runs": selfs,
            "spread_percent": round(spread, 1) if spread is not None else None,
            "heap_bytes": best(heaps),
            "live_bytes": best(lives),
            "runs": repeats,
            "failures": failures,
            "errors": traces,
        }
        results.append(row)

        def show(value, fmt):
            return format(value, fmt) if value is not None else "-"

        note = f"  ({failures} failed)" if failures else ""
        print(f"{row['benchmark']:<22} {show(row['self_seconds'], '10.3f')} "
              f"{show(row['spread_percent'], '8.1f')}% "
              f"{show(row['heap_bytes'], '10,d')} {show(row['live_bytes'], '10,d')}{note}")

    out = ROOT / "doc" / "wren" / "benchmarks-micropython.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({
        "meta": {
            "board": "ESP32-C6FH4 rev v0.2 @ 160 MHz",
            "implementation": version,
            "free_at_boot": free,
            "alloc_at_boot": alloc,
            "repeats": repeats,
            "benchmarks": "benchmarks/python -- identical constants to benchmarks/wren",
            "run": time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime()),
            "note": "heap_bytes is gc.mem_free() before minus after with no forced "
                    "collection, matching what the Wren port measures; live_bytes "
                    "is the same after a gc.collect() and has no Wren counterpart",
        },
        "results": results,
    }, indent=1))
    print(f"\nwritten: {out.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
