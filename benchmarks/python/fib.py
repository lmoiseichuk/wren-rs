# Method dispatch and arithmetic. Same constants as fib.wren: get(24), 5 times.
#
# A classmethod rather than a plain function, because Wren's `get` is a method
# on a class and resolving it is part of what this measures. A module-level
# `def` would be faster and would not be the same benchmark.
import time


class Fib:
    @classmethod
    def get(cls, n):
        if n < 2:
            return n
        return cls.get(n - 1) + cls.get(n - 2)


start = time.ticks_us()
for _ in range(5):
    print(Fib.get(24))
print("elapsed: %f" % (time.ticks_diff(time.ticks_us(), start) / 1000000))
