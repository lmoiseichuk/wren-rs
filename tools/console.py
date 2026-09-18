#!/usr/bin/env python3
"""Console reader for a duty-cycling ESP32-C3 on its built-in USB Serial/JTAG.

    ./tools/console.py                     # 60 s on /dev/ttyACM0
    ./tools/console.py /dev/ttyACM0 600    # ten minutes
    ./tools/console.py /dev/ttyACM0 60 --reset   # pulse EN first

`espflash monitor` is the obvious tool and it does not work here, for two
reasons that both come from the hardware rather than from espflash.

## Reason one: the port is *supposed* to disappear

Every deep sleep unpowers the USB Serial/JTAG PHY, so `/dev/ttyACM0` vanishes
and comes back on each wake (§5). A reader that exits on disconnect therefore
sees at most one wake, and if it happens to attach mid-sleep it sees nothing at
all -- which looks exactly like a dead board. This script loops back to waiting
for the device node instead, and stamps each attach with the unit's USB serial
so a board swap or an unexpected reboot is visible rather than confusing.

## Reason two: attaching a terminal can strap the chip into the ROM downloader

On a board with an external USB-UART chip, DTR and RTS reach EN and GPIO9
through a two-transistor auto-reset circuit. On the C3's *built-in* bridge they
are wired into the die with the same meaning:

    DTR -> GPIO9  (BOOT strapping pin)
    RTS -> EN     (chip reset)

pyserial asserts both on open. So this sets `dtr`/`rts` to False **before**
`open()` -- pyserial applies them at open time, which is the only way to avoid a
strap glitch during enumeration.

## ⚠ Attaching resets the board. Measured, and unavoidable from here.

Opening the CDC port makes the C3 reset -- the raise-then-lower Linux performs on
the control lines when any process opens a tty is close enough to the USB-JTAG
reset sequence to fire one. You will see it in the first line this prints:

    boot: #1 (cold, reset reason 11 = USB, rtc probe 1)

That reset clears RTC memory, so the wake you are watching is a *cold boot*: boot
counter 1, logo replayed, chart ring empty, interval back at its floor, life fit
with no samples. All five look like firmware bugs. None of them is.

So this tool is for watching **one wake in detail**. For anything that depends on
state surviving a sleep -- the chart, the adaptive interval, refresh skipping,
the §7 life fit -- read the **soak log** instead: it is in flash, it survives the
reset, and it records every wake including the ones no console can attend. See
release/FLASHING.md.

On battery there is no host and none of this happens.

## What it detects: the ROM downloader impersonating a hang

    ESP-ROM:esp32c3-api1-20210207
    rst:0x15 (USB_UART_CHIP_RESET),boot:0x5 (DOWNLOAD(USB/UART0/1))
    waiting for download

`boot:0x5` means GPIO9 was low leaving reset, so the chip is in ROM download mode
and your firmware has never run. Usually a **USB power cycle** fixes it.

This is worth naming explicitly because every other signal says the board is
healthy: the port is present and rock stable (steadier than a working board,
which drops off the bus on every wake), `lsusb` is happy, flashing succeeds every
time, a USB meter reads a steady ~0.08 W -- and there is not one byte of output.
Indistinguishable from a hang, a brick, or a firmware bug, and it is none of them.

If a power cycle does not recover it, see `release/FLASHING.md` for the
elimination table (two register reads settle it) before touching the firmware.
"""
import glob
import os
import sys
import time

try:
    import serial
except ImportError:
    sys.exit("pyserial missing:  pip install pyserial")

_args = [a for a in sys.argv[1:] if not a.startswith("--")]
_flags = {a for a in sys.argv[1:] if a.startswith("--")}

PORT = _args[0] if _args else "/dev/ttyACM0"
DURATION = float(_args[1]) if len(_args) > 1 else 60.0
# Opt *in* to resetting. A passive listener is the safe default: the interesting
# runs are the ones already in progress, and a reset would destroy the RTC state
# (boot count, chart ring, learned interval) that this firmware carries across
# sleeps -- the very thing you usually attached in order to watch.
RESET = "--reset" in _flags

START = time.time()

# The ROM prints this when force_download_boot (or a low GPIO9) sent it to the
# downloader. Worth calling out by name, because every other symptom is silence.
DOWNLOAD_MARKERS = ("waiting for download", "DOWNLOAD(USB/UART0/1)")


def stamp():
    return f"[{time.time() - START:8.2f}]"


def usb_unit(port=None):
    """Serial of the debug unit behind `port` -- tells identical boards apart.

    **Resolved from the port**, not by scanning. The first version of this
    globbed every USB device and returned the first MAC-shaped serial it found,
    which is correct only when exactly one Espressif board is attached. With
    several it reports a serial belonging to some other board, and does so in
    the one line an operator reads to confirm they are talking to the right
    device -- reassurance pointing at the wrong machine, which is worse than no
    reassurance at all.
    """
    if port:
        # /sys/class/tty/ttyACM1/device is the USB *interface*; the serial lives
        # on the device a couple of levels up. Walk up until one turns up.
        node = os.path.realpath(f"/sys/class/tty/{os.path.basename(port)}/device")
        for _ in range(4):
            candidate = os.path.join(node, "serial")
            if os.path.exists(candidate):
                try:
                    with open(candidate) as handle:
                        return handle.read().strip()
                except OSError:
                    break
            node = os.path.dirname(node)
    return "?"


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


def attach(port):
    """Open the port without ever asserting DTR (GPIO9 strap) or RTS (EN)."""
    link = serial.Serial()
    link.port = port
    link.baudrate = 115200
    link.timeout = 0.2
    link.dtr = False
    link.rts = False
    link.open()
    _claim(link, port)
    return link


def pulse_reset(link):
    """Pulse EN with the boot strap released -> normal application boot.

    Does *not* clear force_download_boot; nothing in software does.
    """
    link.dtr = False  # GPIO9 stays high -- do not request download mode
    link.rts = True  # EN low
    time.sleep(0.1)
    link.rts = False  # EN released; chip boots
    time.sleep(0.05)


def main():
    print(f"{stamp()} console on {PORT} for {DURATION:.0f}s (reset={RESET})", flush=True)

    total = 0
    attaches = 0
    reset_done = False
    saw_downloader = False

    while time.time() - START < DURATION:
        if not os.path.exists(PORT):
            time.sleep(0.05)
            continue
        try:
            link = attach(PORT)
        except (serial.SerialException, OSError):
            time.sleep(0.1)  # raced enumeration, or someone else holds the port
            continue

        attaches += 1
        print(f"{stamp()} -- attached (#{attaches}, unit {usb_unit(PORT)})", flush=True)

        pending = b""
        try:
            if RESET and not reset_done:
                reset_done = True
                pulse_reset(link)
                print(f"{stamp()} -- EN pulsed (DTR held low)", flush=True)

            while time.time() - START < DURATION:
                chunk = link.read(4096)
                if not chunk:
                    continue
                total += len(chunk)
                pending += chunk
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    text = line.decode("utf-8", "replace").rstrip("\r")
                    print(f"{stamp()} {text}", flush=True)
                    if any(marker in text for marker in DOWNLOAD_MARKERS):
                        saw_downloader = True
        except (serial.SerialException, OSError) as err:
            # Expected on every deep sleep: the PHY loses power mid-read.
            print(f"{stamp()} -- detached ({type(err).__name__})", flush=True)
        finally:
            if pending:  # a partial line, e.g. a prompt with no newline
                print(f"{stamp()} {pending.decode('utf-8', 'replace')}", flush=True)
            try:
                link.close()
            except Exception:
                pass

    print(f"{stamp()} done: {total} bytes across {attaches} attach(es)", flush=True)

    if saw_downloader:
        print(
            "\n"
            "*** The chip is in the ROM downloader, not running your firmware. ***\n"
            "\n"
            "boot:0x5 means GPIO9 was low leaving reset.\n"
            "\n"
            "    -> power-cycle it: unplug and replug USB, then re-run this.\n"
            "\n"
            "If that does not help, see release/FLASHING.md -- two register reads\n"
            "settle whether it is the board rather than the firmware.\n",
            flush=True,
        )
    elif total == 0 and attaches:
        print(
            "\n"
            "Port stayed open the whole time with zero bytes and no detach. A running\n"
            "build should either print, or drop USB when it sleeps. Two candidates:\n"
            "  - the chip is in the ROM downloader (power-cycle USB; see above)\n"
            "  - a non-console build is flashed (field/floor have no console at all)\n",
            flush=True,
        )


if __name__ == "__main__":
    main()
