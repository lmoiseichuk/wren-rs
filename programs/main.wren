// main.wren -- the application the node spends its life in.
//
// `boot` established who the node is and that its calibration is sane. This
// file is the part that runs afterwards and does not stop: sample, filter,
// decide whether the result is worth the radio, and account for what it sent.
//
// It shares no names with `boot` -- see the note at the top of that file -- so
// the split is along a real seam rather than an arbitrary one. Calibration is
// boot's concern; the schedule and the radio are this one's.

// A deterministic noise source.
//
// Deterministic on purpose: this file exists to be *measured*, and a run that
// draws different numbers each time cannot be compared against the one before
// it. A linear congruential generator is the cheapest thing that produces a
// usable spread, and the constants are Park-Miller's.
//
// The arithmetic stays under 2^53 at every step, so it is exact in a double
// and gives the same stream on every part -- which a 32-bit truncation would
// not.
class Noise {
  construct new(seed) {
    _state = seed % 2147483647
    if (_state <= 0) _state = _state + 2147483646
  }

  // The next value in [0, 1).
  next {
    _state = (_state * 16807) % 2147483647
    return (_state - 1) / 2147483646
  }

  // The next value in [low, high).
  between(low, high) { low + next * (high - low) }
}

// A rolling median over the last `width` samples.
//
// **Median, not mean.** A capacitive probe's failure is a spike -- a bad
// contact, or the radio keying while the ADC converts -- and a mean carries a
// single 3300 mV spike into the reported value for as long as the window is.
// A median of five discards two spikes outright and costs a sort of five
// elements, which is nothing next to the conversion that produced them.
class Window {
  construct new(width) {
    _width = width
    _samples = []
  }

  count { _samples.count }
  full { _samples.count >= _width }

  add(value) {
    _samples.add(value)
    if (_samples.count > _width) _samples.removeAt(0)
  }

  // Insertion sort on a copy. The window is five or seven elements, where an
  // insertion sort beats anything with a better asymptote, and sorting a copy
  // keeps the window in arrival order so that `add` stays a push and a shift.
  median {
    if (_samples.isEmpty) return 0
    var sorted = []
    for (value in _samples) {
      var at = 0
      while (at < sorted.count && sorted[at] < value) at = at + 1
      sorted.insert(at, value)
    }
    return sorted[(sorted.count / 2).floor]
  }
}

// Whether a reading is different enough to be worth transmitting.
//
// A node that reports every sample flattens its cell in a season for no
// information: soil moisture moves over hours. So a frame goes out when the
// value has moved by more than the deadband, or when nothing has been sent for
// long enough that the panel would otherwise mark the node missing.
class Deadband {
  construct new(margin, patience) {
    _margin = margin
    _patience = patience
    _last = null
    _since = 0
  }

  // Returns the reason to send, or null to stay quiet. A reason rather than a
  // bool because the accounting at the end is the interesting part: a node
  // whose traffic is all `keepalive` is a node whose deadband is too wide.
  verdict(value) {
    _since = _since + 1
    if (_last == null) return "initial"
    if ((value - _last).abs > _margin) return "moved"
    if (_since >= _patience) return "keepalive"
    return null
  }

  sent(value) {
    _last = value
    _since = 0
  }
}

// One frame as it would go on the air.
class Frame {
  construct new(sequence, value, reason) {
    _sequence = sequence
    _value = value
    _reason = reason
  }

  sequence { _sequence }
  value { _value }
  reason { _reason }

  // Rounded to a tenth on the way out. The extra digits a double carries are
  // not measurement, and spending payload bytes on them is how a frame grows
  // past what a slow radio can hold.
  toString {
    var tenths = (_value * 10).round / 10
    return "#%(_sequence) %(tenths)% (%(_reason))"
  }
}

// The sampling loop, and the accounting that makes a run comparable.
class Node {
  construct new(cycles) {
    _cycles = cycles
    _noise = Noise.new(20260919)
    _window = Window.new(5)
    _band = Deadband.new(1.5, 40)
    _frames = []
    _reasons = {}
  }

  // A slow drift with a spike every so often -- the shape a probe in soil
  // actually produces, rather than white noise around a constant.
  reading(cycle) {
    var drift = 42 + 12 * (cycle / 90).sin
    if (_noise.next < 0.04) return _noise.between(0, 100)
    return drift + _noise.between(-0.8, 0.8)
  }

  run() {
    for (cycle in 0..._cycles) {
      _window.add(reading(cycle))
      if (!_window.full) continue

      var value = _window.median
      var reason = _band.verdict(value)
      if (reason == null) continue

      _band.sent(value)
      _frames.add(Frame.new(_frames.count + 1, value, reason))
      _reasons[reason] = (_reasons[reason] || 0) + 1
    }
  }

  report() {
    System.print("main: %(_cycles) cycles, %(_frames.count) frames")

    // Sorted so the summary reads the same on every run; a map's iteration
    // order is its own business and printing it raw makes two identical runs
    // look different.
    var reasons = _reasons.keys.toList
    for (index in 1...reasons.count) {
      var key = reasons[index]
      var at = index
      while (at > 0 && reasons[at - 1] > key) {
        reasons[at] = reasons[at - 1]
        at = at - 1
      }
      reasons[at] = key
    }
    for (reason in reasons) System.print("  %(reason): %(_reasons[reason])")

    if (!_frames.isEmpty) {
      System.print("  first: %(_frames[0])")
      System.print("  last:  %(_frames[-1])")
    }

    // The duty figure is what the deadband is for, so it is the number worth
    // printing: one frame in fifty is a season on a cell, one in three is a
    // fortnight.
    var duty = (_frames.count * 1000 / _cycles).round / 10
    System.print("  duty:  %(duty)% of cycles carried a frame")
  }
}

var node = Node.new(2000)
node.run()
node.report()
