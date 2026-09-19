# Method dispatch, isolated from allocation. Same constant as method_call.wren:
# n = 10,000, three activations per iteration, then the same again through a
# subclass that calls `super`.
import time


class Toggle:
    def __init__(self, start_state):
        self.state = start_state

    def value(self):
        return self.state

    def activate(self):
        self.state = not self.state
        return self


class NthToggle(Toggle):
    def __init__(self, start_state, max_counter):
        Toggle.__init__(self, start_state)
        self.count_max = max_counter
        self.count = 0

    def activate(self):
        self.count = self.count + 1
        if self.count >= self.count_max:
            Toggle.activate(self)
            self.count = 0
        return self


start = time.ticks_us()
n = 10000
val = True
toggle = Toggle(val)

for _ in range(n):
    val = toggle.activate().value()
    val = toggle.activate().value()
    val = toggle.activate().value()

print(toggle.value())

val = True
ntoggle = NthToggle(val, 3)

for _ in range(n):
    val = ntoggle.activate().value()
    val = ntoggle.activate().value()
    val = ntoggle.activate().value()

print(ntoggle.value())
print("elapsed: %f" % (time.ticks_diff(time.ticks_us(), start) / 1000000))
