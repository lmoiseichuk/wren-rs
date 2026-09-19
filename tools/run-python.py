#!/usr/bin/env python3
"""Send a Python file to a MicroPython board and print what it says.

    tools/run-python.py <by-id path> benchmarks/python/fib.py

**Raw REPL, not paste mode.** Paste mode echoes every line back, which on a
board reading character by character fills its transmit buffer, blocks its
reader, and silently truncates the file — the same failure the Wren console hit,
found the same way. Raw mode (Ctrl-A) echoes nothing and ends with a clean
marker, so a long file arrives whole.

The protocol, for anyone reading this later:

    Ctrl-C Ctrl-C   interrupt whatever is running
    Ctrl-A          enter raw REPL; the board answers with `raw REPL; CTRL-B to exit`
    <source>        sent verbatim
    Ctrl-D          execute; the board answers `OK`, then output, then `\\x04`,
                    then any traceback, then another `\\x04`
    Ctrl-B          back to the friendly REPL
"""

import fcntl
import sys
import time

BAUD = 115200


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    port, path = sys.argv[1], sys.argv[2]

    import serial

    with open(path, "r") as handle:
        source = handle.read()

    link = serial.Serial(port, BAUD, timeout=0.5)
    try:
        fcntl.flock(link.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        raise SystemExit(f"{port} is held by another tool")

    # Opening resets the board on some builds; give it a moment either way.
    time.sleep(1.5)

    link.write(b"\x03\x03")          # interrupt anything running
    time.sleep(0.2)
    link.reset_input_buffer()

    link.write(b"\x01")              # raw REPL
    time.sleep(0.3)
    banner = link.read(256).decode("utf-8", "replace")
    if "raw REPL" not in banner:
        print("warning: no raw REPL banner; got:", banner.strip()[:120],
              file=sys.stderr)

    # **Sent in chunks with a pause.** The board's receive buffer is small and
    # it is parsing as it reads; a single large write overruns it and the tail
    # is lost without any error. This is the same lesson as the Wren port's
    # 4 KB driver buffers, from the other side.
    payload = source.encode("utf-8")
    CHUNK = 128
    for at in range(0, len(payload), CHUNK):
        link.write(payload[at:at + CHUNK])
        link.flush()
        time.sleep(0.02)

    link.write(b"\x04")              # execute
    link.flush()

    # Everything up to the first \x04 is output; after it, a traceback if any.
    collected = b""
    started = time.monotonic()
    while time.monotonic() - started < 600:
        chunk = link.read(1024)
        if chunk:
            collected += chunk
            started = time.monotonic()      # a benchmark can be quiet for a while
            if collected.count(b"\x04") >= 2:
                break

    link.write(b"\x02")              # friendly REPL, so the board is usable after
    link.close()

    text = collected.decode("utf-8", "replace")
    if text.startswith("OK"):
        text = text[2:]
    parts = text.split("\x04")
    output = parts[0]
    error = parts[1] if len(parts) > 1 else ""

    print(output.strip())
    if error.strip():
        print("--- board reported ---", file=sys.stderr)
        print(error.strip(), file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
