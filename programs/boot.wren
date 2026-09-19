// boot.wren -- what a node runs before its application.
//
// The convention is MicroPython's, and deliberately so: the firmware looks for
// `boot` first and `main` second, runs whichever it finds, and treats a
// missing one as "nothing to set up" rather than as a fault. Anybody who has
// put a `boot.py` on a board already knows what this file is for.
//
// **`boot` and `main` share no names.** Each is compiled on its own, and Wren
// reports a top-level name that is never defined as an error at the end of the
// module -- so a class declared here is not visible there. That is a real
// constraint of separate compilation rather than a simplification, and it is
// also what lets either file be replaced without rebuilding the other.
//
// What belongs here is the work that must happen before the application is
// allowed to run at all: establishing who the node is, and refusing to carry
// on with a calibration that would report nonsense.

// The node's identity, as it will appear in every log line.
//
// Built once at start-up because the short name is formatted, not stored, and
// formatting it per message would be the sort of cost that only shows up once
// a node has been in a field for a month.
class Identity {
  construct new(family, revision, serial) {
    _family = family
    _revision = revision
    _serial = serial
  }

  family { _family }
  revision { _revision }
  serial { _serial }

  // Zero-padded to four digits so that names sort the way a human expects --
  // `soil-0007` before `soil-0012`, which plain `toString` gets backwards.
  short {
    var digits = _serial.toString
    while (digits.count < 4) digits = "0" + digits
    return "%(_family)-%(digits)"
  }

  toString { "%(short) rev %(_revision)" }
}

// A sensor curve, as breakpoints of (millivolts, percent).
//
// Breakpoints rather than a polynomial: a field calibration is a handful of
// measured points, and fitting a curve to them adds a step that can be wrong
// in a way nobody can see. Between points this interpolates linearly, which is
// honest about how much it actually knows.
class Curve {
  construct new(points) {
    _points = points
  }

  count { _points.count }

  // Clamped at both ends. A reading outside the calibrated range is not an
  // error -- soil drier than the driest sample is still dry -- but it must not
  // extrapolate, or a disconnected probe reads as 140% moisture.
  at(millivolts) {
    var last = _points.count - 1
    if (millivolts <= _points[0][0]) return _points[0][1]
    if (millivolts >= _points[last][0]) return _points[last][1]

    for (index in 0...last) {
      var low = _points[index]
      var high = _points[index + 1]
      if (millivolts <= high[0]) {
        var span = high[0] - low[0]
        var offset = millivolts - low[0]
        return low[1] + (high[1] - low[1]) * offset / span
      }
    }
    return _points[last][1]
  }

  // A curve is only usable if its voltages rise.
  //
  // Two breakpoints at the same voltage divide by zero inside `at`; a pair
  // that falls inverts the reading and reports a dry pot as soaked. Both are
  // cheap to check here and expensive to notice in a garden.
  check() {
    if (_points.count < 2) Fiber.abort("a curve needs at least two points")
    for (index in 1..._points.count) {
      if (_points[index][0] <= _points[index - 1][0]) {
        Fiber.abort("curve breakpoint %(index) does not rise")
      }
    }
  }
}

// The power-on self-test.
//
// Each check is a closure so that the list reads as what is being tested, and
// so that a failure can name the check that failed rather than a line number
// in a file nobody has on the bench.
class SelfTest {
  construct new() {
    _checks = []
    _failures = 0
  }

  add(name, check) { _checks.add([name, check]) }

  failures { _failures }

  // A failing check is reported and the rest still run. A node that stops at
  // the first fault tells you one thing per power cycle, which turns a
  // ten-minute diagnosis into an afternoon.
  run() {
    for (entry in _checks) {
      var name = entry[0]
      var fiber = Fiber.new(entry[1])
      fiber.try()
      if (fiber.error != null) {
        System.print("  [fail] %(name): %(fiber.error)")
        _failures = _failures + 1
      } else {
        System.print("  [ ok ] %(name)")
      }
    }
    return _failures == 0
  }
}

var identity = Identity.new("soil", "C", 7)

// The shipped calibration: five points measured in air, in dry soil, and at
// three wettings.
//
// **Ordered by voltage, which puts the percentages backwards.** A capacitive
// probe's output *falls* as the soil wets -- more water is more capacitance is
// a lower reading -- so the physically natural order, driest first, descends.
// `at` and `check` both want the voltages to rise, so the table is kept in the
// order they need and the second column is the one that falls.
var curve = Curve.new([
  [1450, 100],
  [1700, 75],
  [2050, 50],
  [2450, 25],
  [2900, 0],
])

System.print("boot: %(identity)")

var test = SelfTest.new()
test.add("curve rises") { curve.check() }
test.add("curve spans the range") {
  if (curve.count < 5) Fiber.abort("only %(curve.count) breakpoints")
}
test.add("dry end clamps") {
  if (curve.at(3300) != 0) Fiber.abort("3300 mV read %(curve.at(3300))%")
}
test.add("wet end clamps") {
  if (curve.at(900) != 100) Fiber.abort("900 mV read %(curve.at(900))%")
}
test.add("midpoint interpolates") {
  var got = curve.at(2250)
  if (got < 36 || got > 39) Fiber.abort("2250 mV read %(got)%")
}

if (test.run()) {
  System.print("boot: self-test passed, handing over to main")
} else {
  System.print("boot: %(test.failures) check(s) failed -- main runs anyway")
}
