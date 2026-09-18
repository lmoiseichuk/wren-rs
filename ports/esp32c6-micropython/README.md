# esp32c6-micropython — the baseline

MicroPython on the same ESP32-C6, measured the same way.

**This is the number worth beating**, because it is what somebody would
otherwise reach for. A Rust VM that is smaller and faster than a C VM is
interesting; one that is smaller and faster than the thing people actually
deploy is useful.

It is equally possible that MicroPython wins on some axis — start-up, or breadth
of library, or sheer maturity. That is a finding and it goes in the table like
any other. A comparison arranged so that only one answer can come out is not a
comparison.

## Status

Not built yet. This one is mostly a flashing exercise: a released build, not a
port to write.

## What to measure

* **Image size** on flash, and static RAM at rest.
* **Free heap after start-up** — the memory a user program actually gets.
* **Start-up time** to a usable prompt.
* **The shared benchmark set**, translated to Python with as little cleverness as
  possible, because a benchmark tuned for one language and transliterated into
  another measures the translation.

## How this differs

Python is not Wren, and the benchmark translation is the weak joint in the whole
comparison. Every benchmark here records the exact source used for both
languages so a reader can judge whether they are the same program.
