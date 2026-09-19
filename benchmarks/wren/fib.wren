// Method dispatch and arithmetic. Upstream uses 28; 24 here, so a three-repeat
// run of two build variants finishes in minutes rather than an hour.
class Fib {
  static get(n) {
    if (n < 2) return n
    return get(n - 1) + get(n - 2)
  }
}

var start = System.clock
for (i in 1..5) {
  System.print(Fib.get(24))
}
System.print("elapsed: %(System.clock - start)")
