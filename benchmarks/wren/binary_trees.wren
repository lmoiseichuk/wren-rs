// Allocation and the collector. Upstream uses maxDepth 12, which needs a
// depth-13 stretch tree -- 16,383 nodes, about 768 KB of live objects. This
// board has 227 KB free after the VM exists, so it reboots on a null store.
// maxDepth 9 keeps the peak near 96 KB. Depth 10 was tried and fails: the
// depth-11 stretch tree and the depth 4, 6 and 8 iterations all complete, then
// it dies building depth-10 trees alongside the live long-lived tree, which measures the collector working
// rather than the collector failing.
class Tree {
  construct new(item, depth) {
    _item = item
    if (depth > 0) {
      var item2 = item + item
      depth = depth - 1
      _left = Tree.new(item2 - 1, depth)
      _right = Tree.new(item2, depth)
    }
  }

  check {
    if (_left == null) return _item
    return _item + _left.check - _right.check
  }
}

var minDepth = 4
var maxDepth = 9
var stretchDepth = maxDepth + 1

var start = System.clock

System.print("stretch tree of depth %(stretchDepth) check: " +
    "%(Tree.new(0, stretchDepth).check)")

var longLivedTree = Tree.new(0, maxDepth)

var iterations = 1
for (d in 0...maxDepth) {
  iterations = iterations * 2
}

var depth = minDepth
while (depth < stretchDepth) {
  var check = 0
  for (i in 1..iterations) {
    check = check + Tree.new(i, depth).check + Tree.new(-i, depth).check
  }

  System.print("%(iterations * 2) trees of depth %(depth) check: %(check)")

  iterations = iterations / 4
  depth = depth + 2
}

System.print("long lived tree of depth %(maxDepth) check: %(longLivedTree.check)")
System.print("elapsed: %(System.clock - start)")
