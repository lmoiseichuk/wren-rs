#!/usr/bin/env python3
"""Re-score a finished run from its stored output, without a board.

    tools/rescore.py

**This is what the golden record buys.** The output of every test is kept, so a
bug in the *scoring* -- as opposed to the running -- can be fixed and the whole
suite re-judged in a second, rather than by occupying the hardware for half an
hour again. The first use was exactly that: the comparator dropped blank lines
from output while keeping them in expectations, and two tests whose output was
character-for-character correct were being reported as failures.
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from importlib import import_module

runner = import_module("run-tests".replace("-", "_")) if False else None

# `run-tests.py` is not an importable name; load it by path instead.
import importlib.util
spec = importlib.util.spec_from_file_location(
    "runtests", Path(__file__).resolve().parent / "run-tests.py")
runtests = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtests)

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "doc" / "wren" / "results.json"

data = json.loads(RESULTS.read_text())
counts = {"pass": 0, "fail": 0, "timeout": 0, "crash": 0, "skip": 0}
changed = 0

for row in data["results"]:
    if row["status"] in ("skip", "crash", "timeout"):
        counts[row["status"]] += 1
        continue
    path = runtests.SUITE / row["test"]
    expected = runtests.Expectations(path)
    status, detail = runtests.compare(expected, row.get("got") or [], "ok")
    if status != row["status"]:
        changed += 1
    row["status"], row["detail"] = status, detail
    counts[status] += 1

data["counts"] = counts
data["rescored"] = True
RESULTS.write_text(json.dumps(data, indent=1))
print(f"{changed} verdict(s) changed")
print(counts)
