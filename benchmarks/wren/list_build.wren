// Building and walking a list. Upstream's `for.wren` uses 1,000,000 elements --
// 8 MB of values on its own, against 227 KB free.
//
// 10,000 here, and the halving from 20,000 is worth recording: a Wren list
// doubles its backing store when it grows, so while it is growing it holds the
// old array *and* the new one. 20,000 elements is ~160 KB steady but ~240 KB
// across the doubling, which does not fit. 10,000 peaks near 120 KB and does.
var list = []

var start = System.clock
for (i in 0...10000) list.add(i)

var sum = 0
for (i in list) sum = sum + i

System.print(sum)
System.print("elapsed: %(System.clock - start)")
