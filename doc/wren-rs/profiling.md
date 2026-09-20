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

## What the interpreter is actually asked to do

A sampling profiler names the code a sample landed in. An opcode histogram
names the work the program asked for, and cannot be misattributed by a fetch
stall. `cargo run --release --features profile,std --example op-profile
<benchmark>` prints one.

| opcode | `method_call` | `fib` | `binary_trees` | `list_build` |
|---|---|---|---|---|
| `Call` | 20.8% | **31.8%** | 21.1% | 20.7% |
| `LoadLocal` | 12.8% | **31.8%** | **26.8%** | **34.5%** |
| `LoadFieldThis` | 15.2% | — | 8.4% | — |
| `Pop` | 13.6% | — | 8.4% | 13.8% |
| `Return` | 10.4% | 9.1% | 8.5% | — |
| `Constant` | 3.2% | 18.2% | — | — |
| `JumpIf` | 4.0% | 9.1% | — | — |
| bytecode instructions | 1,250,137 | 8,252,786 | 3,549,406 | 290,048 |

**Set that beside the machine instructions the board retires and one number
falls out of it:**

| | bytecode ops | instructions retired | machine instructions per opcode |
|---|---|---|---|
| `method_call` | 1,250,137 | 141,955,880 | **113.6** |
| `fib` | 8,252,786 | 936,236,660 | **113.4** |
| `list_build` | 290,048 | 33,547,694 | **115.7** |
| `binary_trees` | 3,549,406 | 527,728,497 | **148.7** |

**A hundred and fourteen machine instructions per bytecode instruction**, and
strikingly constant across three programs of completely different shapes — the
fourth is higher because it is the one that collects. Upstream C Wren runs
`method_call` 6.8 times faster, which puts it near seventeen. That ratio is the
whole gap, stated as work rather than as seconds, and it is the number any
future optimisation should be judged against.

*The arithmetic checks out against the clock, which is worth doing before
believing a derived figure: 1.25 million opcodes at 113.6 instructions and 2.59
cycles per instruction is 368 million cycles, and `method_call` at 160 MHz
takes 2.30 s — 368 million cycles.*

`Call` is between a fifth and a third of every profile, and in `fib` it ties
`LoadLocal` almost exactly — because arithmetic in Wren *is* method calls, so
`a - b` dispatches. That makes the call path, in both its forms, the first
thing worth understanding at this level of detail.

---

## What one construct costs

The 114-instructions-per-opcode average says nothing about what to change: it
does not tell you whether a call costs forty or four hundred. `tools/measure-rs.sh
speed --bin opcost` prices constructs directly. Each program is the same loop
with one thing added to the body, run at N and 2N iterations with the
difference taken -- which cancels compilation, start-up and everything else
that happens once, leaving exactly the loop, N times.

| body | per iteration | over the empty loop | what it adds |
|---|---|---|---|
| *(empty loop)* | 733 | — | the `while`, its compare and its increment |
| `i` | 795 | **62** | `LoadLocal`, `Pop` |
| `i + i` | 1,025 | **292** | `LoadLocal` ×2, `Call(+)`, `Pop` |
| `b.get` | 1,245 | **512** | `LoadLocal`, `Call`, `LoadFieldThis`, `Return`, `Pop` |
| `b.get + b.get` | 1,925 | **1,192** | that twice, plus a `Call(+)` |

Solving those gives the three numbers worth carrying around:

| | machine instructions |
|---|---|
| an opcode that only moves a value | **~31** |
| a primitive call (`a + b`) | **~199** |
| a closure call and its return | **~410** |

**Thirty-one instructions to move one value** is the number that matters,
because it is nearly all dispatch rather than work: fetching the byte, the jump
table, reading an operand, indexing the stack, pushing, and going round. It
sets the price of everything -- and it is what makes a peephole pass valuable,
since fusing two opcodes into one saves that whole thirty-one every time it
fires, whatever the two were.

It was 39 before the collection check moved behind a cached boolean, and the
empty loop was 814. Pricing the loop again after a change is the quickest way
to see whether it touched dispatch or only one arm.

---

## Where the work is now

After the day's changes -- the collection check behind a boolean, the chunk
moved rather than cloned, the upvalue early-out and five fused opcode pairs --
the shape has changed enough to be worth restating.

| | bytecode ops | instructions retired | per opcode |
|---|---|---|---|
| `method_call` | 1,000,051 | 122,933,098 | 122.9 |
| `fib` | 6,377,116 | 815,452,532 | 127.9 |

**Per opcode went up, and that is the fusion working**: what it removed were
the cheap instructions, so what remains is denser. Total work fell on every
benchmark. The per-opcode figure is only comparable between builds that fuse
the same pairs.

`Call` is now **26% of `method_call`'s instructions and 41% of `fib`'s**, and
the price list says a primitive call is ~199 machine instructions and a closure
call with its return ~410 -- unchanged by anything so far, because nothing so
far has touched them.

**And the cost inside a call is the lookup, not the work.** Sampling `fib`,
which is 41% `Call`:

| samples of 60 | |
|---|---|
| 27 | `run_frames` (dispatch) |
| **7** | **`Vm::find_method`** |
| 6 | `Chunk::read_short` |
| 4 | `Vec<Value>::push` |
| **3** | **`Vm::class_of`** |
| 3 | `Vec<Frame>::push` |
| 2 | `Vm::call_target` |
| **2** | the `+` primitive itself |

The arithmetic is two samples. Finding out *which* `+` to run is ten. That is
what `find_method` costs: `heap.class(id)` is a table slot holding a `Box`,
the `Box` holds the `ObjClass`, the `ObjClass` holds a `Vec` of method entries,
and the entry is a fourth load -- four dependent loads before anything happens,
and a dependent load is what an in-order core cannot hide.

**So the next thing to try is a method cache**: a small direct-mapped table
from `(class, symbol)` to the decoded entry, invalidated when a method is
bound, which is rare. One load and a compare against four dependent loads.

The pairs left to fuse are diffuse -- the largest is `Call -> Call` in `fib` at
11.8%, and fusing across a call is not something a pass can do -- so a second
round of fusion is worth much less than the first.

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

## Two things the counter refused

Both of these looked obviously right, were written, passed the suite, and were
thrown away because instructions retired went **up**. Neither would have been
settled by the clock: both are under one per cent, which is well inside what
placement moves.

**Upstream's result placement.** A primitive's result goes into the receiver's
slot and the stack top drops -- `args[0] = result; stackTop -= numArgs - 1` --
where this VM truncates and then pushes, paying a capacity test. Adopting it
cost 0.17% more work on `method_call` and 0.38% more on `binary_trees`.

The reason is instructive. Upstream can write the slot unconditionally because
a call that re-enters the interpreter is a *different method type* there
(`METHOD_FUNCTION_CALL`), and never reaches that code. Here `Fn.call` and the
Sequence methods taking a block are ordinary primitives, so they can return
with the stack shorter than the receiver slot -- without a length test this was
an index-out-of-bounds on every closure call. That test is the whole saving and
then some. **An optimisation copied from another implementation carries its
surrounding design with it**, and the part that made it free was somewhere else
in the file.

**Hoisting a value computed twice.** `let receiver_at = self.stack.len() -
arity - 1` appears on both sides of a `class_of` call, and `class_of` takes
`&self`, so the compiler cannot prove the stack did not move and recomputes it.
Computing it once before the branch cost 0.49% more on `method_call` and 0.96%
more on `fib`.

Recomputing two arithmetic instructions is cheaper than keeping a value live
across a call, because live across a call means spilled and reloaded. The
compiler was already making the better choice, and "this is computed twice" is
not on its own a reason to change it.

---

**Cheaper operand reads, in three forms, all refused.** Each operand byte of an
instruction is bounds-checked separately, and `read_short` reads two. Taking a
fixed-size window over the code once per instruction should replace all of that
with a single check, in safe Rust, because indexing a `&[u8; N]` with a
constant needs no check at all. The chunk is padded so a window always exists;
`Chunk::seal` does that where the compiler finishes a function and where a file
is read.

| form | `method_call` | `fib` | `binary_trees` |
|---|---|---|---|
| **copied** window, `*chunk.window(at)` | +29.1% | +27.4% | +21.9% |
| **borrowed** window via `first_chunk` | +2.76% | +1.75% | +3.18% |
| borrowed window via a constant-width range | +1.21% | −0.44% | +1.84% |

The first is the instructive one. An instruction starts wherever it starts, so
the window is unaligned, and copying eight unaligned bytes on this part is
eight byte loads and eight byte stores -- not the two word loads the shape of
`[u8; 8]` suggests. That is a 27% regression from a line that looks free.

The other two say the idea does not pay even done properly, and the reason is
that **the checks were mostly not there to begin with**: `read_short` reads
`code[offset]` and `code[offset + 1]` in one basic block, and the optimiser
merges those into a single check of the larger index. So a window saves at most
one check per instruction and costs a pointer kept live across the arm, which
in a function this size means register pressure. Roughly a wash, measured as a
small loss.

*The general shape again: the cost that was being removed had already been
removed by the compiler, and the only way to find that out was to build it.*
The padding, `seal` and `code_len` are not in the tree.

---

## A full-Wren pass over the top ten opcodes

Counted with `op-profile` over all four benchmarks, three runs. The counts are
a property of the bytecode, so the three agreed exactly -- which is worth
running anyway, because a disagreement would mean the profiler and not the
program. 10,428,514 instructions in total:

| | opcode | total | share |
|---|---|---|---|
| 1 | `Call` | 3,696,226 | 35.44% |
| 2 | `LoadLocalPair` | 1,030,338 | 9.88% |
| 3 | `JumpIf` | 1,021,004 | 9.79% |
| 4 | `LoadLocalConstant` | 948,959 | 9.10% |
| 5 | `Constant` | 790,317 | 7.58% |
| 6 | `LoadLocalReturn` | 545,158 | 5.23% |
| 7 | `Return` | 524,494 | 5.03% |
| 8 | `LoadFieldThis` | 378,056 | 3.63% |
| 9 | `LoadLocal` | 281,425 | 2.70% |
| 10 | `StoreFieldThisPop` | 278,718 | 2.67% |

### What the counter is good for at this size

Two runs of the *same binary* differed by 67 instructions in 426 million --
one part in six million. That is what makes a 0.3% result meaningful here, and
it was worth establishing before trusting any of the numbers below.

### The significance rule

**A delta under one per cent is reproducible but not attributable.** Two runs
of the same binary differ by 67 instructions in 426 million, so a 0.3% result
is real in the sense that it will repeat -- but it is register allocation and
instruction scheduling responding to an unrelated edit, not the mechanism the
change was about. Counting only cases at or past 1%, and judging a change on
the **sum across all four benchmarks**, is what separates the two.

Re-scoring the refused list below against that rule is uncomfortable reading:

| refused change | cases past 1% | verdict under the rule |
|---|---|---|
| upstream's result placement | 0 of 2 | not shown harmful, worst +0.38% |
| hoisting `receiver_at` | 0 of 2 | not shown harmful, worst +0.96% |
| operand window, copied | 3 of 3 | genuinely refused |
| operand window, borrowed | 3 of 3 | genuinely refused |
| operand window, const range | 2 of 3 | genuinely refused |
| method cache, either form | 3 of 3 | genuinely refused |

The first two were decided on evidence that this rule calls churn. Both were
re-measured in the current tree and both still lost on the sum -- result
placement by +0.08% -- so the conclusions stand, but they stand on the sum and
not on the individual numbers recorded beside them.

### What was kept

**The field offset in a local, and the constant pool beside the code.**
`field_offset` was already on the frame rather than walked out of the heap, but
every field instruction still reached `self.frames.last()` for it. `constants`
was reached through the `Rc` and then the `Vec` on a sixth of all instructions.
Both are now locals in `run_frames`, beside `base`, `module` and `units`.

| | `binary_trees` | `fib` | `list_build` | `method_call` | all four |
|---|---|---|---|---|---|
| field offset only | −0.32% | **+1.24%** | −0.07% | −0.96% | +0.44% |
| constants only | +0.13% | +0.50% | −0.29% | +0.14% | +0.31% |
| **both** | **−0.81%** | +0.31% | **−0.85%** | **−1.50%** | **−0.30%** |

**Neither is worth having alone, and together they are.** Each on its own is a
net loss across the four; the pair wins on three of them. `fib` is the one that
does not improve, and it is the benchmark that touches no field at all -- so it
pays the register for `field_offset` and gets nothing back. Why adding a second
hoisted slice then *reduces* `fib`'s regression, from +1.24% to +0.31%, is a
register-allocation effect and not something the source makes visible.

The lesson is the one this page keeps arriving at from a new direction:
**the unit of measurement is the combination that ships, not the idea.**

### One capacity test for two values

`LoadLocalConstant` and `LoadLocalPair` each push twice, and between them they
are a fifth of everything the benchmarks execute. Two `push`es test the
capacity twice and set the length twice; `extend_from_slice` of a two-element
array does both once.

| | `binary_trees` | `fib` | `list_build` | `method_call` | sum |
|---|---|---|---|---|---|
| with the field offset and constants | **−1.28%** | −0.93% | **−1.44%** | **−1.65%** | **−1.14%** |

Three of four past the threshold, none regressing, 13.3 million instructions.

**And it was nearly shipped broken.** Fusing the two reads ahead of both pushes
is wrong for `LoadLocalPair`: its second slot can be the slot the first push
writes into, so reading both first reads that slot's stale contents. `var a3 =
a2` is exactly that shape -- the initializer's value lands in `a3`'s slot, and
the next statement loads `a3`. The fix names the one slot where the two orders
disagree, which costs a compare against the capacity test it saves.
`LoadLocalConstant` has no such dependency, its second value coming from the
constant pool.

What makes this worth writing down is how badly it hid:

  - **146 unit tests passed.** So did the four benchmarks, and so did the
    device measurement -- which is how a −1.40% number was produced for a
    build that was wrong.
  - **The conformance suite aborted with no message at all.** It installs a
    silent panic hook, so the index panic unwound into a second panic and the
    process died on `SIGABRT` with an empty stderr and, because stdout was
    block-buffered into a file, no output either. It reads exactly like a
    hang or a toolchain fault.
  - **The first regression test written for it passed on both versions.**
    A test that cannot fail is worse than no test, because it converts a gap
    into a claim of coverage.

The case was found by making the bug *safe but wrong* -- `get().unwrap_or` in
place of the indexing panic -- so the suite could name the files instead of
dying: `language/variable/many_locals.wren` and its sibling, which need 255
locals to say it. `a_fused_local_pair_sees_the_value_it_just_pushed` says it in
five, and fails on the buggy version -- which was checked, this time, before
the test was believed.

### A constant error is not worth a `Result`

`field_of` and `set_field` returned `Result<_, RuntimeError>`, and a
`RuntimeError` is a `String` and a line -- twenty-four bytes of return value
where sixteen would do, on a path the benchmarks take three quarters of a
million times and which fails in none of them. Every failure is the same
sentence, so the accessors now report with `Option` and `bool` and the caller
builds the error.

On its own that was nearly a wash, and instructively so:

| | `binary_trees` | `fib` | `list_build` | `method_call` | sum |
|---|---|---|---|---|---|
| `Option` and `bool` | −2.85% | **+3.70%** | +0.11% | **−8.18%** | +0.13% |
| ...and the error constructor `#[cold]` | **−3.84%** | +0.31% | +0.07% | **−8.70%** | **−2.04%** |

**`fib` touches no field at all**, so a 3.7% swing on it was never about field
access: removing the `Result` moved the error *construction* inline into every
field arm, and `run_frames` is large enough that a few bytes per arm decide
what stays in registers. Marking the constructor `#[cold]` and `#[inline(never)]`
puts it back out of line, and the benchmark that cannot use the change stops
paying for it.

The pattern is worth naming, because it is the third time this page has hit it
from a different direction: **a change to a hot arm is also a change to the
size of the function containing it**, and in a function this size that second
effect can be larger than the first and point the other way. It is not visible
in the source and not predictable from the mechanism. Only the four numbers
say which way it went.

### Where the `#[cold]` trick stops working

Taking a constant error out of line won `field_of` two points. The same edit
was then tried on every other eager `RuntimeError::new` the call and return
paths construct, and every one of them lost:

| error moved out of line | `binary_trees` | `fib` | sum |
|---|---|---|---|
| `not_a_field`, from six arms of `run_frames` | −3.84% | +0.31% | **−2.04%** |
| the `Call` arm's two, already behind `ok_or_else` | +0.31% | +0.69% | +0.55% |
| `call_target`'s two | +0.34% | **+1.37%** | +0.89% |
| `resume_chunk`'s one | +0.81% | **+2.00%** | +1.47% |

**The boundary is which function the error was in, not how hot it was.**
`field_of` is called from six arms of a function large enough that a few bytes
per arm decide what stays in registers. `call_target` and `resume_chunk` are
small, are inlined into the call and return paths, and having the construction
inline is apparently what the optimiser wants there -- rewriting it as an
`Option` return, marking it `#[cold]`, and forcing `#[inline(always)]` all
produced *bit-identical* machine code, and all three cost `fib` the same 1.37%.
Three formulations, one answer: leave it where it is.

### The ceiling on what is left

At 108 machine instructions per bytecode instruction, what a change can be
worth is mostly decided before it is written:

| | share of executions | 10 machine instructions saved on each |
|---|---|---|
| the top ten opcodes | 91.0% | 8.4% |
| the next ten | 9.0% | **0.83%** |
| `Call` alone | 35.4% | 3.3% |

The model is worth trusting: it predicted 2.05% for the field change, against
2.0% measured. What it says is that **the next ten opcodes cannot reach one per
cent** -- 9% of executions, and a simple arm has nowhere near ten instructions
to give. The field path only managed thirty because it was constructing a
`String`-bearing error that no benchmark ever raises.

It also says where anything further has to come from: `Call` is the only opcode
frequent enough that three instructions saved is a whole per cent. The known
lever there is quickening, and that needs mutable chunks -- which is what
`code_units` and `chunk_constants` trade on.

### What was refused, again

Two changes were written, measured, and found to be already in the list below:
writing a primitive's result into the receiver's slot instead of truncating and
pushing, and hoisting the `receiver_at` that is computed twice. Both were
refused by an earlier pass, for reasons recorded here, and both measured as
losses again -- within a tenth of a per cent of the numbers already written
down.

That is a good sign for the instrument and a bad use of a day. **Read the
refused list before optimising, not after.** It is the section immediately
below this one, and it exists precisely so that a plausible idea does not have
to be paid for twice.

### What was refused as a no-op

Fetching the field offset through `get` rather than `last().map_or` -- removing
an `Option` branch that the compiler turns out to have already removed.
Identical instruction counts, to within the noise floor established above. The
same shape as the operand-window result further down: *the cost being removed
had already been removed.*

## The method cache, built twice and refused

`find_method` was the largest named item in the profile of `fib` -- 7 samples
of 60 against 2 for the `+` primitive it was looking up -- and it is four
dependent loads: a table slot holding a `Box`, the `Box`, the `ObjClass`'s
`Vec` of entries, and the entry. Caching that is the textbook answer.

**The measurements first**, because they say the cache should work:

| | lookups | distinct `(class, symbol)` pairs | call sites | monomorphic |
|---|---|---|---|---|
| `method_call` | 270,020 | **19** | 38 | **100%** |
| `fib` | 2,625,874 | **11** | 18 | **100%** |
| `binary_trees` | 750,321 | **20** | 67 | 86.7% |

Millions of lookups asking a dozen distinct questions, and essentially every
call site asks the same one every time. Sixty-four slots hit **99.98% or
better** on all four benchmarks. A per-call-site cache, which would need a
bytecode change, could not do better than that.

**And it was slower.** Twice:

| | `method_call` | `fib` | `binary_trees` |
|---|---|---|---|
| 64-bit key, two-multiply hash | +4.64% | +6.58% | +3.31% |
| one-word key, one-multiply hash | +3.54% | +4.97% | +2.53% |

Time moved the same way: `fib` 13.843 s to 14.552 even in the cheaper form.

**The reason is that there is no memory hierarchy to defeat.** A method cache
is worth having on a machine where chasing four pointers means four chances to
miss cache and stall for a hundred cycles. This part executes from flash
through a cache but keeps *data* in SRAM with nothing in front of it, so every
one of those loads costs the same two or three cycles whether it is the first
or the fortieth. Four cheap loads beat a hash, a key comparison and a load,
and no hit rate can change that.

*This is the sharpest instance of the rule this whole page is about: the
optimisation was chosen from a profile that named the right function, sized
with measurements that said it would hit, and it still lost -- because the
reason the textbook recommends it does not hold on this part. The profile said
where the time was. Only the experiment said whether the fix helped.*

The instrument that sized it is still here -- `op-profile` reports the distinct
pairs and the monomorphic share -- because the same question comes up for
quickening, which rewrites a call site to its resolved form rather than looking
it up in a table, and would face exactly this arithmetic.

---

## Where the interpreter's time goes now

After a day of it, `method_call`'s profile is flat: `run_frames` itself is 21
samples of 50 and nothing else is above 3. `binary_trees` has one peak left,
`Heap::collect` at 6 of 50, which is the collector rather than the interpreter.

**What is left is fetch, not execute.** Cycles per instruction sit at 2.58,
and the changes that moved the clock furthest were the ones that made the code
*smaller* rather than the ones that made it do less:

| change | work | time |
|---|---|---|
| fusing five opcode pairs | −5.7% | −7.3% |
| moving cold code out of the loop | −0.8% | −2.7% |
| padding branch targets | 0% | −3.9% |

The last of those removes no instructions at all. `run_frames` compiles to
17,698 bytes against a 32 KB instruction cache which also serves every
primitive it calls, and the whole hot set does not comfortably fit.

**So the levers that remain are about size, and they are getting thin.**
`Op::Closure` and `Op::Class` are rare and long and look like obvious things to
move out of line; doing it made the function 48 bytes *bigger*, because LLVM
had already outlined them. The cheap error paths were worth 2,800 bytes and
2.5% of the clock; there is no second batch of that size.

---

## A fixed-length instruction set, built and refused

`Closure` is the only variable-length instruction: an opcode, a `u16`
constant, a `u8` count, and then two bytes per upvalue. That makes it a special
case in every walker over the code -- the disassembler, the file writer, the
jump-target scan, the peephole pass -- and puts a loop inside the dispatch
arm.

There is no rule that one construct compiles to one instruction, so it was
split: a fixed four-byte `Closure` followed by one fixed two-byte capture
instruction per upvalue, each appending to the closure on top of the stack.
Every instruction in the set then has a length known from its opcode alone.

It works -- 829/829, and upvalues survive a `.wrenc` round trip -- and it is
slower, in two independent encodings:

| | `binary_trees` | `fib` | `method_call` |
|---|---|---|---|
| two opcodes (`CaptureLocal`, `CaptureUpvalue`) | +0.27% | +0.79% | +0.62% |
| one opcode, local flagged in the operand's top bit | +0.23% | +0.79% | +0.56% |

Time moved further: `method_call` 1.949 s to 2.005 and 2.012, `fib` 13.454 to
14.008 and 14.102.

**The cost is not in executing the new instructions.** `fib` runs `Closure`
about twice in a whole benchmark, and its work went up by 6,374,871
instructions against 6.38 million bytecode instructions executed -- almost
exactly one extra machine instruction per dispatch. Adding an arm to a
forty-arm match perturbed how the optimiser compiles the whole loop, and every
instruction that is *not* a capture paid for it. `run_frames` grew from 17,698
bytes to 18,020 and 18,152.

*The rule this hands forward: an opcode is not free even when it never runs.*
Fusing pairs was worth it because it removed a sixth of all dispatches, which
paid for the five opcodes it added many times over. Splitting one rare
instruction into two adds the same kind of cost with nothing to set against it.

**What it would have bought was real but not measurable**: `instruction_len`
becomes a pure table, and four walkers lose a special case. That is worth
having when it is free, and it is not free here.

### What a uniform width would cost

Measured statically over the compiled benchmarks -- `op-profile` reports it.
The current encoding is already variable and already narrow: an instruction is
one, two or three `u16` units, and about three quarters of them are two.

| | `binary_trees` | `method_call` | `fib` | `list_build` |
|---|---|---|---|---|
| as compiled | 788 B | 630 B | 220 B | 212 B |
| every instruction 4 B | +13% | +18% | +13% | +19% |
| every instruction 6 B | +69% | +77% | +69% | +78% |

**And four bytes is not actually a candidate.** `binary_trees` and `fib` both
contain three-unit instructions, so the only uniform width that holds every
instruction in them is six -- which is where the +69% to +78% row is, not the
+13% one. Against that, the decoding a fixed layout would simplify is not
where the time goes: the operand-window experiment above found the optimiser
had already merged the bounds checks it would remove.

*These numbers replace an earlier table that read +35% to +46%. The instrument
was wrong, not the conclusion: `Chunk::footprint` returned `code.capacity()`
-- a count of `u16` units -- under a doc comment saying bytes, and `op-profile`
summed unit lengths into a variable it printed as `B` and then compared
against byte-based encodings. So the "as compiled" column was half its true
size and every percentage above it was inflated by the same factor. The
refusal stands on the corrected figures; it stood on the wrong ones by
accident.*

### A benchmark filter that changed the benchmark

`tools/measure-rs.sh --only <names>` was added to cut a measurement round from
ninety seconds to twenty. It selected benchmarks with `option_env!`, which
means it **changed the image** -- and the image is what is being measured.

Same commit, same source, `binary_trees`:

| the binary was built to run | instructions retired |
|---|---|
| `binary_trees,fib` | 411,792,094 |
| `method_call,fib,binary_trees` | **426,918,702** |

3.67% apart, reproducibly, from a difference that does nothing at run time.

**Two optimisations were refused on that difference** -- popping without a
bounds check, and dropping the dead length from the code slice -- because
their measurements were taken with one filter and compared against a baseline
taken with another. Re-measured against a baseline built the same way, the
first is worth −0.19% to −0.44% across all three benchmarks and the second is
exactly neutral. Both were fine.

Two things follow, and the first is the general one:

**A measurement harness that is compiled into the thing it measures is part of
the measurement.** `--pad` gets this right -- it is a linker flag, applied to
every build under comparison, and its effect was itself measured (700
instructions in 411.8 million). `--only` got it wrong by being invisible: it
looked like a reporting convenience and was a code change.

So every build now runs all four benchmarks and `--only` filters the *output*.
The loop is slower and the comparison is sound.

**And `binary_trees` is not the erratic benchmark it appeared to be.** Every
unexplained swing attributed to it in this session was this. With the filter
out of the image it reproduces like the others, and the earlier note here
claiming it varies between builds -- and the rule built on that note, to treat
it as a veto rather than a signal -- was wrong and is withdrawn.

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
