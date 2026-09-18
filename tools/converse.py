#!/usr/bin/env python3
"""Send several console commands over one attach.

    tools/converse.py <by-id path> <reply-seconds> <command> [command ...]
    SETTLE=30 tools/converse.py <port> 20 health      # wait longer before sending

`say.py` is the one-command tool and it is the right one most of the time. This
exists for the two cases it cannot cover.

**Several commands in one attach.** Attaching resets an ESP32-C3 and the reset
clears RTC memory, so a sensor that has just been attached to is always a
cold-booted one -- it broadcasts for the clock and cannot unicast. Reaching a
window that unicasts, or a second radio window of any kind, needs more than one
command in the same session.

**The C5 panel.** `say.py` waits for a ready marker and then sends; on the panel
that handshake does not land and every command is silently ignored. It looks
exactly like a board whose console cannot be read, and was diagnosed as one --
wrongly -- four times. This drains for a fixed time and then sends, which works.
`SETTLE` is that drain, in seconds, for a board whose boot work runs long.
"""
import os, sys, time, serial

BAUD = 115200
port, window = sys.argv[1], float(sys.argv[2])
commands = sys.argv[3:]


# --- one holder per port ------------------------------------------------------
#
# **Two tools on one serial port do not queue, they corrupt.** pyserial opens the
# device happily a second time and both readers then get a share of the bytes, so
# a command's reply lands split between them or not at all. Seen on the bench as
# `device reports readiness to read but returned no data (multiple access on
# port?)` -- which reads exactly like a dead board, and was diagnosed as one.
#
# An advisory `flock` on the device makes that a refusal instead. It is
# non-blocking on purpose: a cron job that cannot have the port should say so and
# leave, not pile up behind an interactive session that may run for an hour.
#
# The lock lives on the *file descriptor*, so it is released when the process
# exits for any reason -- including being killed -- with nothing to clean up.
import fcntl as _fcntl


def _claim(link, port):
    try:
        _fcntl.flock(link.fileno(), _fcntl.LOCK_EX | _fcntl.LOCK_NB)
    except OSError:
        link.close()
        raise SystemExit(
            f"{port} is held by another tool -- refusing to share it.\n"
            "  Two readers on one port split the bytes between them, which "
            "presents as a dead board.\n"
            "  Wait for it, or stop the other session."
        )


link = serial.Serial()
link.port, link.baudrate, link.timeout = port, BAUD, 0.2
link.dtr = link.rts = False
link.open()
_claim(link, port)


def drain(seconds):
    out = b""
    end = time.time() + seconds
    while time.time() < end:
        chunk = link.read(4096)
        if chunk:
            out += chunk
    return out.decode("utf-8", "replace")


try:
    print(drain(float(__import__("os").environ.get("SETTLE","12")) ), end="", flush=True)
    for command in commands:
        print(f"\n-- sending: {command}", flush=True)
        link.write((command + "\n").encode())
        link.flush()
        print(drain(window), end="", flush=True)
finally:
    link.close()
