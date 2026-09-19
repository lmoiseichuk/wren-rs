# Building and walking a list. Same constant as list_build.wren: 10,000.
#
# `list.append` in a loop rather than a comprehension: Wren has no
# comprehension, and using one here would measure a language feature the other
# side cannot answer.
import time

values = []

start = time.ticks_us()
for i in range(10000):
    values.append(i)

total = 0
for i in values:
    total = total + i

print(total)
print("elapsed: %f" % (time.ticks_diff(time.ticks_us(), start) / 1000000))
