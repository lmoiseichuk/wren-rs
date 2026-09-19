#!/usr/bin/env python3
"""Run upstream Wren's test suite against a board, and score it.

    tools/run-tests.py <by-id path> [group ...]
    tools/run-tests.py <by-id path> core/list language/if

**Why not upstream's `util/test.py`.** That one launches a process per test and
reads its exit status. Here there is no process and no exit status: there is one
board, one serial port, and a REPL. So this keeps a single session open, sends
each file between `.run` and `.end`, and reads back what the device printed
between two markers.

The expectation syntax is upstream's, deliberately — `// expect:`,
`// expect error`, `// expect runtime error:`, `// skip:`, `// nontest`. Parsing
it the same way is what makes "passes the suite" mean the same thing here as it
does there.

Results are written as JSON for `tools/report-tests.py` to turn into the pages
under `doc/wren/`.

**`doc/wren/results.json` is a golden record, not just a scoreboard.** It keeps
what the board printed for every test, passing ones included, so that when the
Rust implementation runs the same suite its output can be diffed line by line
against what the C reference actually said -- with no reflash of the reference
and no second run of it. "Both passed" is a weak claim; "both printed the same
thing" is the one worth making, and it is only available if the output was kept
the first time.
"""

import fcntl
import json
import os
import re
import sys
import time
from pathlib import Path

BAUD = 115200

# Upstream's patterns, from vendor/wren/util/test.py.
EXPECT = re.compile(r"// expect: ?(.*)")
EXPECT_ERROR = re.compile(r"// expect error(?! line)")
EXPECT_ERROR_LINE = re.compile(r"// expect error line (\d+)")
EXPECT_RUNTIME_ERROR = re.compile(r"// expect (handled )?runtime error: (.+)")
SKIP = re.compile(r"// skip: (.*)")
NONTEST = re.compile(r"// nontest")
IMPORT = re.compile(r'^\s*import\s+"([^"]+)"')

BEGIN = "<<<wren-begin>>>"
END = "<<<wren-end>>>"

ROOT = Path(__file__).resolve().parent.parent
SUITE = ROOT / "vendor" / "wren" / "test"


class Expectations:
    """What a test file says should happen."""

    def __init__(self, path: Path):
        self.path = path
        self.output = []          # expected printed lines, in order
        self.compile_error = False
        self.runtime_error = None
        self.skip = None
        self.nontest = False

        for line in path.read_text(errors="replace").splitlines():
            if NONTEST.search(line):
                self.nontest = True
                return
            match = SKIP.search(line)
            if match:
                self.skip = match.group(1)
                return
            match = EXPECT.search(line)
            if match:
                self.output.append(match.group(1))
                continue
            if EXPECT_ERROR.search(line) or EXPECT_ERROR_LINE.search(line):
                self.compile_error = True
                continue
            match = EXPECT_RUNTIME_ERROR.search(line)
            if match:
                self.runtime_error = match.group(2)


def modules_for(path: Path, seen=None):
    """Every module a test imports, transitively, as (name, source) pairs.

    **The board has no filesystem, so imports have to be carried to it.** A test
    that says `import "./module"` sits in a directory next to `module.wren`;
    this finds that file, and anything it imports in turn, so the whole set can
    be registered before the test runs.

    The name is the literal import string. No `resolveModuleFn` is installed on
    the device, so that string is exactly the key `loadModuleFn` is asked for --
    keeping the two the same is what makes the lookup work without reimplementing
    upstream's path resolution.
    """
    if seen is None:
        seen = {}
    for line in path.read_text(errors="replace").splitlines():
        match = IMPORT.match(line)
        if not match:
            continue
        name = match.group(1)
        if name in seen:
            continue
        # `./module` is relative to the importing file's directory.
        candidate = (path.parent / (name[2:] if name.startswith("./") else name))
        candidate = candidate.with_suffix(".wren")
        if not candidate.exists():
            # A core module like "meta" or "random", or a deliberately missing
            # one -- either way the device handles it, not us.
            continue
        seen[name] = candidate.read_text(errors="replace")
        modules_for(candidate, seen)
    return seen


class Board:
    """One serial session, held open for the whole run.

    Re-attaching per test would reset the board, and a reset costs a boot and a
    fresh VM every time -- minutes across nine hundred tests, and it would also
    lose the very state a reset is supposed to prove nothing depends on.
    """

    def __init__(self, port: str):
        self.port = port
        self.reconnects = 0
        self._open()

    def _open(self):
        import serial  # imported late so --help works without pyserial

        self.link = serial.Serial(self.port, BAUD, timeout=0.2)
        try:
            fcntl.flock(self.link.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            raise SystemExit(f"{self.port} is held by another tool")
        # Opening the port resets the board; let it boot and drain the banner.
        time.sleep(2.0)
        self.link.reset_input_buffer()

    def reconnect(self):
        """Wait for the board to come back, and reopen.

        **A test that crashes the board is a result, not an accident.** The
        `limit/` group exists to push Wren past what it can do, and on this part
        "past what it can do" means an out-of-memory reboot -- which drops the
        USB device, invalidates the file descriptor, and killed the first full
        run after twenty-four tests. Surviving it is the difference between a
        suite that reports a limit and one that stops at it.
        """
        self.reconnects += 1
        try:
            self.link.close()
        except Exception:
            pass
        # The device re-enumerates; the by-id path reappears when it does.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if os.path.exists(self.port):
                try:
                    self._open()
                    return True
                except Exception:
                    pass
            time.sleep(0.5)
        return False

    def register_modules(self, modules):
        """Upload each module the next test will import."""
        import serial
        try:
            self.link.write(b".modules\n")   # forget the previous test's
            self.link.flush()
            time.sleep(0.05)
            for name, body in modules.items():
                self.link.write(f".module {name}\n".encode())
                for line in body.splitlines():
                    self.link.write(line.encode("utf-8", "replace") + b"\n")
                    self.link.flush()
                self.link.write(b".end\n")
                self.link.flush()
                time.sleep(0.05)
            self.link.reset_input_buffer()
            return True
        except (serial.SerialException, OSError):
            return self.reconnect()

    def sync(self):
        """Wait until the board is quiet, and throw away anything pending.

        **Without this, one slow test corrupts the next.** When a test times out
        or reboots the board, the device is still emitting while the host has
        moved on -- so the following `.run` arrives mid-stream and its source
        buffer inherits the previous test's tail. It showed up as a five-line
        program reporting an error on line 215, and as `Error at 'run': Expect
        end of file`, which is the literal command text ending up inside the
        program.
        """
        import serial
        deadline = time.monotonic() + 3.0
        quiet_since = time.monotonic()
        while time.monotonic() < deadline:
            try:
                chunk = self.link.read(512)
            except (serial.SerialException, OSError):
                return self.reconnect()
            if chunk:
                quiet_since = time.monotonic()
                continue
            # A fifth of a second with nothing said means it has finished.
            if time.monotonic() - quiet_since > 0.2:
                break
        try:
            self.link.reset_input_buffer()
        except (serial.SerialException, OSError):
            return self.reconnect()
        return True

    def run(self, source: str, timeout: float = None, module: str = None):
        """Send one program, return what it printed.

        Returns `(lines, status)` where status is `"ok"`, `"timeout"` or
        `"crashed"`. A timeout is not an error -- some `limit/` tests are meant
        to run a long time. A crash is the board rebooting under the test, which
        is itself a finding.
        """
        import serial

        # **Scaled to the file, not a flat number.** At 115200 baud a 60 KB
        # `limit/` test takes most of ten seconds just to arrive, before Wren
        # has compiled a line of it -- and a flat 20 s reported ten of those as
        # timeouts, which is the transfer being measured rather than the VM.
        if timeout is None:
            timeout = 20.0 + len(source) / 4000.0

        self.sync()
        try:
            # **The module name decides whether foreign methods bind.**
            # Upstream's dispatcher only answers for modules under `./test/`
            # (`vendor/wren/test/api/api_tests.c:9`), so an `api/` test run as
            # `main` gets no bindings however many are compiled in.
            command = b".run\n" if module is None else f".run {module}\n".encode()
            self.link.write(command)
            for line in source.splitlines():
                self.link.write(line.encode("utf-8", "replace") + b"\n")
                # The device parses per line; a burst overruns its receive
                # buffer and the tail of a long file goes missing.
                self.link.flush()
            self.link.write(b".end\n")
            self.link.flush()
        except (serial.SerialException, OSError):
            return [], "crashed" if self.reconnect() else "lost"

        collected = []
        started = time.monotonic()
        seen_begin = False
        pending = b""
        while time.monotonic() - started < timeout:
            try:
                chunk = self.link.read(512)
            except (serial.SerialException, OSError):
                return collected, "crashed" if self.reconnect() else "lost"
            if not chunk:
                continue
            pending += chunk
            # A reboot announces itself; catching it here is faster and surer
            # than waiting for the read to fail.
            if b"rst:0x" in pending or b"Guru Meditation" in pending:
                self.reconnect()
                return collected, "crashed"
            while b"\n" in pending:
                raw, pending = pending.split(b"\n", 1)
                line = raw.decode("utf-8", "replace").rstrip("\r")
                if BEGIN in line:
                    seen_begin = True
                    collected = []
                    continue
                if END in line:
                    return collected, "ok"
                if seen_begin:
                    collected.append(line)
        return collected, "timeout"


def compare(expected: Expectations, got, run_status):
    """Score one test. Returns (status, detail)."""
    if run_status == "timeout":
        return "timeout", "no end marker within the timeout"
    if run_status == "crashed":
        return "crash", "the board rebooted running this test"
    if run_status == "lost":
        return "crash", "the board did not come back"

    # The device prints its own verdict line for a failed interpret.
    #
    # **Empty lines are kept.** `// expect:` with nothing after it is a real
    # expectation -- `System.printAll([])` prints an empty line and the test says
    # so. Filtering blanks out of the output while leaving them in the
    # expectations made `core/system/print_all` and `core/string/replace` fail
    # with output that was character-for-character correct.
    result_line = next((l for l in got if l.startswith("[result] ")), None)
    # **The port's own lines are not the program's output.** `[cost]` reports
    # time, heap and stack for every run; counting it as printed output made
    # every API test fail with "1 unexpected extra line" while its output was
    # exactly right.
    printed = [l for l in got
               if not l.startswith("[result] ") and not l.startswith("[cost] ")]
    # A single trailing blank is the console's own newline, not the program's.
    while printed and printed[-1] == "" and len(printed) > len(expected.output):
        printed.pop()

    if expected.compile_error:
        if result_line == "[result] compile error":
            return "pass", ""
        return "fail", f"expected a compile error, got {result_line or 'success'}"

    if expected.runtime_error is not None:
        # **A runtime error raised from the C side does not set the interpret
        # result.** The `api/` tests that drive Wren with `wrenCall` report
        # their error through the error callback after `wrenInterpret` has
        # already returned success, so the printed text is the only evidence.
        raised = any(l.startswith("runtime error:") for l in got)
        if result_line == "[result] runtime error" or raised:
            return "pass", ""
        return "fail", f"expected a runtime error, got {result_line or 'success'}"

    if result_line is not None:
        return "fail", f"unexpected {result_line[9:]}"

    if printed == expected.output:
        return "pass", ""

    # First divergence, which is far more useful than a whole diff.
    for index, want in enumerate(expected.output):
        if index >= len(printed):
            return "fail", f"line {index + 1}: expected {want!r}, got nothing"
        if printed[index] != want:
            return "fail", f"line {index + 1}: expected {want!r}, got {printed[index]!r}"
    return "fail", f"{len(printed) - len(expected.output)} unexpected extra line(s)"


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    port = sys.argv[1]
    groups = sys.argv[2:]

    files = sorted(SUITE.rglob("*.wren"))
    if groups:
        files = [f for f in files
                 if any(str(f.relative_to(SUITE)).startswith(g) for g in groups)]
    if not files:
        raise SystemExit("no tests matched")

    board = Board(port)
    results = []
    counts = {"pass": 0, "fail": 0, "timeout": 0, "crash": 0, "skip": 0}

    for index, path in enumerate(files, 1):
        relative = str(path.relative_to(SUITE))
        expected = Expectations(path)

        if expected.nontest or expected.skip:
            counts["skip"] += 1
            results.append({"test": relative, "status": "skip",
                            "detail": expected.skip or "nontest",
                            "expected": [], "got": [],
                            "expect_compile_error": False,
                            "expect_runtime_error": None})
            continue

        source = path.read_text(errors="replace")
        imports = modules_for(path)
        if imports:
            board.register_modules(imports)
        # `api/` and `benchmark/api_*` need upstream's module naming to reach
        # the foreign-method dispatcher; everything else runs as `main`.
        module = None
        if relative.startswith("api/") or relative.startswith("benchmark/api_"):
            module = "./test/" + relative[:-len(".wren")]
        got, run_status = board.run(source, module=module)
        status, detail = compare(expected, got, run_status)
        counts[status] += 1
        # **What the board actually printed is kept, for every test.** A
        # pass/fail column says whether Wren agreed with itself; the output says
        # what it did, and that is what the Rust implementation will be
        # compared against line by line. Storing it only for failures would mean
        # re-running the whole suite the first time a passing test's output
        # turned out to matter.
        results.append({
            "test": relative,
            "status": status,
            "detail": detail,
            "expected": expected.output,
            "got": got,
            "expect_compile_error": expected.compile_error,
            "expect_runtime_error": expected.runtime_error,
        })

        mark = {"pass": ".", "fail": "F", "timeout": "T",
                "crash": "!", "skip": "s"}[status]
        sys.stdout.write(mark)
        sys.stdout.flush()
        if index % 72 == 0:
            sys.stdout.write(f"  {index}/{len(files)}\n")
            sys.stdout.flush()
            # **Checkpoint.** Nine hundred tests over a serial line is half an
            # hour; an interrupted run that saved nothing would mean starting
            # again, and the first thing anyone does with a long run is
            # interrupt it.
            checkpoint = ROOT / "doc" / "wren" / "results.json"
            checkpoint.parent.mkdir(parents=True, exist_ok=True)
            checkpoint.write_text(json.dumps(
                {"counts": counts, "partial": True, "results": results}, indent=1))

    print()
    total = sum(counts.values())
    print(f"{counts['pass']}/{total} passed, {counts['fail']} failed, "
          f"{counts['timeout']} timed out, {counts['crash']} crashed the board, "
          f"{counts['skip']} skipped")
    if board.reconnects:
        print(f"the board rebooted {board.reconnects} time(s) during the run")

    out = ROOT / "doc" / "wren" / "results.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({
        "counts": counts,
        "meta": {
            "board": "ESP32-C6FH4 rev v0.2 @ 160 MHz",
            "port": "ports/esp32c6-wren",
            "wren": "vendor/wren 0.4.0, unmodified",
            "run": time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime()),
        },
        "results": results,
    }, indent=1))
    print(f"written: {out.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
