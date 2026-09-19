# Allocation and the collector. Same constants as binary_trees.wren: maxDepth 9.
#
# No `__slots__`, deliberately. It would make Python's objects cheaper than
# Wren's instances and flatter the result; a plain attribute dict is the closer
# analogue of a Wren instance with fields.
import time


class Tree:
    def __init__(self, item, depth):
        self.item = item
        self.left = None
        self.right = None
        if depth > 0:
            item2 = item + item
            depth = depth - 1
            self.left = Tree(item2 - 1, depth)
            self.right = Tree(item2, depth)

    def check(self):
        if self.left is None:
            return self.item
        return self.item + self.left.check() - self.right.check()


min_depth = 4
max_depth = 9
stretch_depth = max_depth + 1

start = time.ticks_us()

print("stretch tree of depth %d check: %d"
      % (stretch_depth, Tree(0, stretch_depth).check()))

long_lived_tree = Tree(0, max_depth)

iterations = 1
for _ in range(max_depth):
    iterations = iterations * 2

depth = min_depth
while depth < stretch_depth:
    check = 0
    for i in range(1, iterations + 1):
        check = check + Tree(i, depth).check() + Tree(-i, depth).check()
    print("%d trees of depth %d check: %d" % (iterations * 2, depth, check))
    iterations = iterations // 4
    depth = depth + 2

print("long lived tree of depth %d check: %d" % (max_depth, long_lived_tree.check()))
print("elapsed: %f" % (time.ticks_diff(time.ticks_us(), start) / 1000000))
