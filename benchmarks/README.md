# The embedded benchmark set

Upstream Wren's benchmarks, scaled to a part with 512 KB of RAM — and the same
programs in Python, with the same constants, for MicroPython.

## Why these exist rather than upstream's

**Upstream's set is written for desktops and most of it does not fit.** Measured
on an ESP32-C6 with ~227 KB of heap free after the VM exists:

| upstream benchmark | what it asks for | outcome |
|---|---|---|
| `for` | a 1,000,000-element list — 8 MB of values | null store, board reboots |
| `binary_trees` | a depth-13 stretch tree — 16,383 nodes, ~768 KB | null store, board reboots |
| `map_numeric`, `fibers` | likewise | board reboots |

Not one of those is a Wren failure. They are programs larger than the machine,
and running them unchanged would report "the VM crashed" where the truth is
"this workload needs a workstation".

So each is scaled down by its *size* parameter and nothing else: same algorithm,
same shape, same operations — fewer of them. The constant that changed is named
at the top of every file, so the scaling is visible rather than buried.

## The rule

**The Wren and Python versions use identical constants.** If `binary_trees.wren`
builds to depth 9, so does `binary_trees.py`. A comparison where one language
was handed a smaller problem is not a comparison, and the easiest way to produce
a flattering number by accident is to tune one side while porting it.

Where the languages genuinely cannot do the same thing, the file says so at the
point it occurs. There is one such case and it matters: **Wren has a single
numeric type, so every number is a double.** On a part with no hardware floating
point that is soft-float, where MicroPython uses small integers. The arithmetic
benchmarks therefore measure a representation difference as much as an
interpreter one, and reading them as purely the latter would be wrong.

## Sizing

`binary_trees` is the one with a hard ceiling, because `stretchDepth` is
`maxDepth + 1`:

| `maxDepth` | long-lived | stretch peak | fits? |
|---|---|---|---|
| 9 | ~48 KB | ~96 KB | **yes** |
| 10 | ~96 KB | ~192 KB | marginal, against 227 KB |
| 11 | ~192 KB | ~384 KB | no |

Depth 9 was chosen so the run measures the collector working rather than the
collector failing.
