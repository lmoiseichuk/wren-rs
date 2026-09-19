# Profiling: the instruments, and what each one lies about

How this VM is measured, in the order it was learned. [`design.md`](design.md)
carries the decisions; [`memory.md`](memory.md) carries what the heap profiler
found. This page is about the measuring itself, because on this part that
turned out to be the hard half.

There are four instruments, and **every one of them gave a confidently wrong
answer before it gave a right one**:

| instrument | what it answers | what it got wrong first |
|---|---|---|
| a workstation | does it run, and does it answer correctly | said every real optimisation was noise |
| a sampling profiler under gdb | which code the time is spent in | attributes to the wrong line, and cannot see a stall |
| the board's clock | how long a build takes | 3-4% of it is where the code landed, not what it does |
| the chip's performance counter | how much work a build does | reads `minstret` as an illegal instruction |

The corrections are recorded beside the readings, because a correction is
cheaper to reuse than the experiment that produced it.

---

## The host is not a witness

Every published number here is the C6's. A workstation is for correctness, and
for nothing else.

**The reason is the pipeline, not the clock.** An out-of-order core issues past
a load that has not returned; an in-order RISC-V stops. This VM reaches every
object through a bounds-checked index into a `Vec`, so its hot path is a chain
of dependent loads — receiver to class, class through its box, class to method
table, method to closure, closure to function, function to chunk. That is
exactly the shape a big out-of-order window hides and a small in-order one pays
for in full.

Measured: the change that removed six of ten heap lookups per call was **inside
the noise on a workstation** and was `method_call` 2.766 → 2.356 s on the board.
A laptop would have said it was not worth making.

So the host runs `cargo run --release --example bench`, which takes about
0.18 s, and answers one question: do all four programs produce the right
answers. Timing it is not meaningful — a single run is inside the noise of
whatever else the machine is doing between two processes. `--repeat N` and the
*minimum* of the runs is the only host timing worth reading, and it is for
narrowing a search, never for a claim.

---

## The sampling profiler, and what it cannot see

**There is no `perf` here.** `/proc/sys/kernel/perf_event_paranoid` is `3`,
which forbids even own-process events. What is available is `ptrace`
(`/proc/sys/kernel/yama/ptrace_scope` is `0`), and that is enough for a poor
man's profiler: attach, take a backtrace, detach, repeat. The innermost frame
of each sample is where the time was.

```sh
cargo build --release --features std --example bench
./target/release/examples/bench --repeat 300 >/dev/null &
TARGET=$!
for _ in $(seq 1 50); do
    gdb -p "$TARGET" -batch -ex bt 2>/dev/null
    kill -0 "$TARGET" 2>/dev/null || break
done > samples.txt
kill "$TARGET"

grep -oE '^#0 .*' samples.txt \
    | sed -E 's/^#0 +(0x[0-9a-f]+ in )?//; s/ \(.*//' \
    | sort | uniq -c | sort -rn | head -15
```

**`--repeat` is not optional.** One run of the benchmark set is 0.18 s, which
is shorter than a single attach-and-detach; the first attempt at this produced
an empty file, and an empty file looks exactly like a profiler that does not
work.

A current reading, 50 samples:

| samples | innermost frame |
|---|---|
| 12 | `Vm::run_frames` |
| 4 | `Vm::find_method` |
| 4 | `quicksort::partition_lomuto_branchless_cyclic::<(u32, u32, u32)>` |
| 3 | `Vm::call_target` |
| 2 | `Heap::instance_fields` |
| 2 | `Heap::instance_field` |
| 2 | `Heap::collect` |
| 2 | `Vec<Value>::pop` |

That third row is the interesting one and is a live lead: it is
`runs.sort_unstable()` inside `compact_fields`, sorting about 1,500 entries at
every major collection — 8% of host samples for a sort nothing outside the
collector reads. The allocation that built that list has been removed; the sort
has not.

**Three things this instrument gets wrong, all of which have cost time here:**

1. **It reports the frame, and inlining moves the frame.** `line_at` never
   appeared in any profile because it inlines into its caller; what appeared
   was the instruction fetch it sat inside. The cost was real and the name was
   not.
2. **It attributes a mispredicted branch to the next instruction.** In an
   interpreter the dispatch line therefore absorbs the cost of every arm's
   final indirect jump, which makes the decode look far more expensive than it
   is. `Op::from_byte` measured 14% of `binary_trees` this way, and rewriting
   it to a single jump table turned out to be worth nothing once code placement
   was controlled for — see [`design.md`](design.md).
3. **It cannot see a stall at all.** A cycle spent waiting for an instruction
   to arrive from flash is attributed to whichever instruction eventually
   retires. On this part that is two cycles in three, and no amount of sampling
   will say so.

So: a sampling profile is for **generating candidates**, never for accepting
one. Everything it suggests is then measured on the board.

---

## The board is exact

Worth stating plainly, because it is unusual and it changes what a single run
is worth: **there is no run-to-run noise.** Three flashes of the same image
gave `binary_trees`

```
9.006731 s   9.006731 s   9.006731 s
```

— identical to the microsecond, not to three decimals. One run is the
measurement. A difference between two runs of the same image does not happen,
so if two numbers differ, the binaries differ.

That is what made the next finding possible, and also what made it necessary.

---

## …but where its code lands is not

**A change to a field of `Heap` moved `fib` by 1.8%.** `fib` allocates four
kilobytes in total, compacts nothing, and cannot be touched by a change to the
collector. All four benchmarks moved by about that much in the same direction.

The explanation is not what the change did; it is that the change moved
everything after it in the instruction stream. Shown directly, by changing
*only* placement — `-Cllvm-args=-align-all-nofallthru-blocks=N` pads every
branch target to a 2^N-byte boundary and does nothing else to the program:

| branch targets padded to | `binary_trees` | `fib` | `list_build` | `method_call` | flashed image |
|---|---|---|---|---|---|
| nothing | 9.007 s | 16.774 s | 0.5659 s | 2.4788 s | 465,904 B |
| 2 B | 9.007 s | 16.774 s | 0.5659 s | 2.4788 s | 465,904 B |
| **4 B** *(the default now)* | 8.729 s | **16.127 s** | 0.5446 s | **2.3820 s** | **472,544 B** |
| 8 B | 8.737 s | 16.127 s | **0.5445 s** | 2.3820 s | 484,832 B |
| 16 B | **8.725 s** | 16.137 s | 0.5446 s | 2.3837 s | 511,232 B |
| 32 B | 8.751 s | 16.174 s | 0.5445 s | 2.3901 s | 559,776 B |

Two bytes is a no-op and the image comes out **byte-identical**, because
RV32IMAC's compressed instructions already force that alignment — which is a
useful control in itself: a knob that should do nothing, doing nothing.

**Aligning function entries is not the same lever and is worth almost nothing.**
`-align-all-functions=5` moves the four benchmarks by 0.12%. What matters is
the branch targets *inside* the dispatch loop, not where the loop begins. This
also rules out the obvious targeted fix: `#[repr(align(N))]` on a function does
not exist on stable Rust, `#[link_section]` is an unsafe attribute that
`#![forbid(unsafe_code)]` rejects, and neither would help if it worked.

**The consequence for method.** A source change that alters the size of
anything in the interpreter also moves everything after it, and that alone is
worth a couple of per cent in either direction. So a one-or-two-per-cent
difference between two builds is not evidence about the change — and
*consistency across all four benchmarks is not evidence either*, because that
is precisely what placement produces. The reasoning "it moved all four by the
same amount, which is what a per-instruction cost looks like" is wrong, and was
used here before this was understood.

---

## The control needed a control of its own

If the code's placement matters, does the data's? The mirror experiment leaks a
fixed number of bytes before the VM is built, so every allocation the VM makes
moves by that much while its code stays exactly where it was.

**The first version measured nothing, and said so only because it was
checked.** Shifts of 0, 16 and 64 bytes gave times identical to the
microsecond — which is what a real "data placement does not matter" result
looks like, and also what a leak that never happened looks like. Printing the
address of the next allocation settled it: `0x408013a0` in every case. The
optimiser had deleted a `Vec::with_capacity` whose result nothing read.

The fix is to write to the buffer and print its address, so the allocation is
observed and cannot be removed:

```rust
let mut shift: Vec<u8> = Vec::new();
shift.resize(HEAP_SHIFT, 0xa5);
let leaked = shift.as_ptr() as usize;
// ...print it, then forget it
```

With that, the next allocation really does move — `0x408013a0` → `0x408013e0`
for a 64-byte shift — and the answer holds:

| `binary_trees` | shift 0 | shift 64 B |
|---|---|---|
| time | 9.006461 s | 9.007241 s |

0.009%, and `fib` 0.0002%. **Data placement does not matter here.** That is
what the part predicts: instructions are fetched from flash through a cache,
while data sits in SRAM with nothing in front of it, so data has no alignment
to get wrong.

*The general lesson is worth more than the result: a control that reports "no
effect" has two explanations, and only one of them is an answer. Check that the
control did something before believing it found nothing.*

---

## The chip's performance counter

Time cannot distinguish "this does less work" from "this landed better". A
count of instructions retired can, because it does not move when the code
moves.

**The standard RISC-V counters are not implemented.** Reading `minstret` raises
an illegal-instruction exception — `mtval=0xb8202bf3`, which is a read of CSR
`0xb82`. What *is* implemented is Espressif's own, three custom CSRs that
ESP-IDF declares for this chip with `SOC_CPU_HAS_CSR_PC` and saves across a
sleep:

| CSR | name | holds |
|---|---|---|
| `0x7e0` | `mpcer` | which events to count, one bit each |
| `0x7e1` | `mpcmr` | bit 0 enables counting |
| `0x7e2` | `mpccr` | the count |

### Finding out what the events are

ESP-IDF names exactly one, `PCER_CYCLES = 1 << 0`. The rest are in the chip's
manual and in no header on this bench, so they were found by experiment:
`ports/esp32c6-wren-rs/src/bin/counters.rs` runs a baseline loop and four
variants that each add one known operation, and reports which event moves by
one. Counts per iteration:

| event | base | +arith | +load | +store | +branch | what it is |
|---|---|---|---|---|---|---|
| 0 | 7.05 | 8.03 | 9.01 | 8.04 | 14.04 | **cycles** |
| 1 | 7.00 | 7.00 | 9.00 | 8.00 | 11.00 | **instructions retired** |
| 4 | 0.00 | 1.00 | 0.00 | 0.00 | 2.00 | ? |
| 5 | 1.00 | 1.00 | **2.00** | 1.00 | 1.00 | **loads** |
| 6 | 1.00 | 1.00 | 1.00 | **2.00** | 2.00 | **stores** |
| 7 | 0.00 | 0.00 | 0.00 | 0.00 | 1.00 | ? |
| 8 | 1.00 | 1.00 | 1.00 | 1.00 | 2.00 | ? |
| 9 | 1.00 | 1.00 | 1.00 | 1.00 | 0.00 | ? |
| 10 | 4.00 | 4.00 | 5.00 | 4.00 | 7.00 | ? |
| 12 | 0.00 | 0.00 | 0.00 | 0.00 | 1.00 | ? |

Events 2, 3, 11, 14 and 15 read zero throughout and are probably not
implemented.

**Two readings in that table are not yet explained**, and are written down
rather than smoothed over. `+arith` does not move event 1, though it adds an
operation — almost certainly because the optimiser folds that add into the
loop's induction variable, which would also explain why it costs a cycle on
event 0 without retiring an instruction. And `+branch` takes event 9 from 1 to
0, which no reading of "event 9 counts something" explains on its own.

**The one worth finding next is an instruction-cache miss.** It is very likely
among events 4 and 7-12. That would turn the fetch-bound conclusion below from
an inference into a direct reading, and would say whether shrinking the
interpreter is the lever or whether something else is.

### The proof that it measures work and not placement

Same source, built twice, differing only in branch-target padding:

| `binary_trees` | no padding | 4 B padding | difference |
|---|---|---|---|
| time | 9.007332 s | 8.731564 s | **−3.06%** |
| instructions retired | 541,532,864 | 541,532,863 | **1 in 541 million** |

| `fib` | no padding | 4 B padding | difference |
|---|---|---|---|
| time | 16.774597 s | 16.127337 s | **−3.86%** |
| instructions retired | 970,748,140 | 970,747,973 | 1 in 5.8 million |

Three per cent of time, and the work identical to a part in a million. It
cannot be fooled by where the code landed, which is the entire point.

### What it says about the bottleneck

Cycles per instruction, from the device's own timer around `interpret` and a
nominal 160 MHz — so approximate, but the comparison between columns is not:

| | no padding | 4 B padding |
|---|---|---|
| `binary_trees` | 2.67 | **2.58** |
| `fib` | 2.77 | **2.66** |
| `list_build` | 2.72 | **2.62** |
| `method_call` | 2.69 | **2.59** |

**Between two and three cycles per instruction on an in-order core whose common
instructions take one.** Two of every three cycles retire nothing, and the
thing that moves the number is where the branch targets sit. That is a
fetch-bound interpreter.

It explains a result that is otherwise baffling: rewriting the dispatch from
two jump tables to one is 2.4-3.4% faster on an unpadded build and a 1.1-1.6%
*regression* on a padded one. Both changes treat the same bottleneck, so they
are substitutes rather than additions, and the bigger code of the rewrite costs
more than its saved jump table returns. It also predicts that replicated
dispatch — the stable-Rust stand-in for computed `goto`, which re-expands the
decode at the end of every arm — will lose for the same reason, since its whole
method is to make the loop bigger.

---

## The heap profiler

`cargo run --release --features profile --example heap-profile` counts the
population by type, what collection costs, how much young garbage survives, and
verifies the write barriers. It is a host tool and legitimately so: **what it
measures is counts, not time**, and a count does not care what it is running
on.

It is the instrument behind every memory decision, and what it found — that
collection is 16.9% of the one benchmark that allocates and 0.0-0.6% of the
other three, which capped what any replacement collector could win — is in
[`memory.md`](memory.md).

---

## Which benchmark to optimise against, and in what order

The four are not interchangeable, and two of them answer questions the others
confound. For interpreter work the order is `method_call`, then `fib`, then
`binary_trees`, and `list_build` is a correctness check rather than a
stopwatch.

| | time | instructions | heap | what it isolates |
|---|---|---|---|---|
| `method_call` | 2.382 s | 147,936,064 | 8,572 B | **dispatch, almost purely.** Barely allocates, so the collector cannot move it; what it measures is method lookup and the call sequence |
| `fib` | 16.127 s | 970,747,973 | 4,172 B | **calls and arithmetic**, and the longest run of the four, so its clock is the steadiest |
| `binary_trees` | 8.729 s | 541,532,863 | 117,384 B | **allocation and collection.** The right benchmark for memory work and the wrong one for dispatch work, because a change in either shows up here |
| `list_build` | 0.545 s | 33,547,740 | 132,844 B | correctness, and peak memory |

**`list_build` is too short to time.** Half a second means a one per cent
change is five milliseconds, and placement alone moves it by three per cent, so
its clock cannot settle anything. Its *instruction count* is still exact — that
is the point of the counter — so it remains usable for "did this do less work",
just not for "is this faster". Its real value is peak memory, where it is the
heaviest of the four.

**`binary_trees` should not be the default target for interpreter work**, which
it has been by habit because it is the one the profiler has most to say about.
It holds a thousand-node tree and collects 124 times in a run, so a dispatch
change and a collector change land on the same number and cannot be told apart.
Use it when the collector *is* the subject.

---

## The protocol, as it now stands

| the change is… | measure it by |
|---|---|
| expected to change memory | the heap profiler, then peak on the board |
| expected to be large (>5%) | one board run is enough; the board is exact |
| expected to be small (1-3%) | **instructions retired**, which placement cannot move |
| a build or toolchain flag | the board's clock, at several paddings |
| suggested by a host profile | nothing yet — that is a candidate, not a finding |

And two rules that came out of getting this wrong:

- **A number that only exists in seconds, for a change under about 3%, is not a
  finding.** Say what it measured and that it is inside placement, or measure
  the work.
- **Check that a control did something.** "No effect" and "the experiment did
  not run" are the same reading.

---

## Running any of it

```sh
# the board, as committed
tools/measure-rs.sh                       # speed profile
tools/measure-rs.sh size                  # footprint profile

# the board, at a chosen placement -- the control for a small change
tools/measure-rs.sh speed --pad 0
tools/measure-rs.sh speed --pad 4

# which events the chip's counter implements
tools/measure-rs.sh speed --bin counters

# the host: correctness, and candidates
cargo run -p wren --release --example bench
cargo run -p wren --release --features profile --example heap-profile
```

The benchmark runner prints instructions retired beside each result, so the
placement-invariant figure is there without asking for it.
