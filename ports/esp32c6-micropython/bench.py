# The shared benchmark set, translated from Wren.
#
# **The translation is the weak joint in this whole comparison**, so it is kept
# deliberately literal: the same algorithm, the same constants, the same shape,
# and no Python idiom that Wren has no equivalent for. A list comprehension or a
# generator here would measure Python's cleverness rather than its interpreter,
# and the Wren side has no way to answer it.
#
# Where the two languages genuinely differ, the difference is noted at the point
# it occurs, because those are the places a reader should discount the number.
#
# The Wren originals are in ../esp32c6-wren/main/main.c.

import gc
import time


# --- fib: method dispatch and arithmetic ------------------------------------
#
# Wren:
#   class Fib {
#     static of(n) { n < 2 ? n : of(n - 1) + of(n - 2) }
#   }
#   System.print(Fib.of(24))
#
# A classmethod rather than a bare function, to keep the dispatch: Wren's `of`
# is a method on a class and resolving it is part of what the benchmark
# measures. A module-level `def` would be measurably faster and would not be
# measuring the same thing.
class Fib:
    @classmethod
    def of(cls, n):
        return n if n < 2 else cls.of(n - 1) + cls.of(n - 2)


def bench_fib():
    return Fib.of(24)


# --- tree: allocation and the collector -------------------------------------
#
# Wren:
#   class Tree {
#     construct new(depth) { ... }
#     sum { _depth == 0 ? 1 : 1 + _left.sum + _right.sum }
#   }
#   for (i in 1..40) { total = total + Tree.new(10).sum }
#
# `__slots__` is *not* used, deliberately. It would make Python objects cheaper
# than Wren's and flatter the result; a plain attribute dict is the closer
# analogue of a Wren instance with fields.
class Tree:
    def __init__(self, depth):
        self.depth = depth
        if depth > 0:
            self.left = Tree(depth - 1)
            self.right = Tree(depth - 1)

    def sum(self):
        if self.depth == 0:
            return 1
        return 1 + self.left.sum() + self.right.sum()


def bench_tree():
    total = 0
    for _ in range(40):
        total += Tree(10).sum()
    return total


# --- loop: interpreter dispatch ---------------------------------------------
#
# Wren:
#   for (i in 1..200000) { x = x + i % 7 }
#
# **The one place the languages are not doing the same arithmetic.** Wren has a
# single numeric type and every value here is a double, so on a part with no
# hardware floating point this is soft-float. MicroPython uses small integers,
# which are far cheaper. The comparison is still worth making -- it is what each
# language actually does with the same program -- but the gap here is a
# representation difference and not an interpreter one, and reading it as the
# latter would be wrong.
def bench_loop():
    x = 0
    for i in range(1, 200001):
        x = x + i % 7
    return x


BENCHMARKS = (
    ("fib", bench_fib),
    ("tree", bench_tree),
    ("loop", bench_loop),
)


def run():
    print("=== esp32c6-micropython ===")
    try:
        import sys
        print("impl: ", sys.implementation)
    except Exception:
        pass

    gc.collect()
    print("mem:   %d B free at start" % gc.mem_free())

    for name, fn in BENCHMARKS:
        gc.collect()
        before = gc.mem_free()
        started = time.ticks_us()
        result = fn()
        elapsed = time.ticks_diff(time.ticks_us(), started)
        lowest = gc.mem_free()
        gc.collect()
        recovered = gc.mem_free()
        print("bench: %-6s %8d us   peak %6d B   result %s" %
              (name, elapsed, before - lowest, result))
        if recovered + 64 < before:
            print("bench: %-6s did not return %d B" % (name, before - recovered))


if __name__ == "__main__":
    run()
