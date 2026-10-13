#!/usr/bin/env python3
"""Where the `wren` crate's flash goes, in a bytecode-only `-Os` build.

    tools/uwren-breakdown.py                    # build HEAD and attribute it
    tools/uwren-breakdown.py --rev 7e864d3      # build an older commit
    tools/uwren-breakdown.py --as-shipped       # the port's own wren features
    tools/uwren-breakdown.py --elf <path>       # attribute an ELF, build nothing
    tools/uwren-breakdown.py --detail           # every symbol, under its row
    tools/uwren-breakdown.py --rules 2026-09-20 # the first measurement's rules

This is the method behind the table in the README's µwren section. The first
version of that table (111,640 B, 2026-09-20, commit 7e864d3) was produced by
a one-off command that was not recorded in the repository.
`--rev 7e864d3 --rules 2026-09-20` reproduces every row of it to the byte,
and a total of 111,698 B: 58 B more,
which are two small generic functions -- `<&str as Display>::fmt` and
`RangeInclusive<usize>::next` -- that this build keeps out of line and that
one inlined. Which of the two happens depends on the checkout's path; see
`build`. An image built from another directory gave 111,640 exactly.

**What it builds.** `ports/esp32c6-wrenc-rs`, the binary of the same name,
`cargo build --profile size` -- `opt-level = "z"`, fat LTO, one codegen unit,
`panic = "abort"`, which is the "-Os" the README means. That port runs `.wrenc`
and links no compiler. It is built in a detached `git worktree` of `--rev`
under the temporary directory, with its own target directory, and both are
removed afterwards (`--keep` keeps them); its path is fixed by the commit, for
the reason `build` gives. Nothing in the checkout is touched.

**Which `wren` it builds.** The 2026-09-20 image had `wren` with `alloc` and
`libm` -- 64-bit numbers, the transcendentals from `libm`, the full core
library. The port has since been switched to `uwren`, which is `nofp`: an
integer build, a different language and a much smaller `Num`. So by default the
worktree's copy of the port's manifest has the `wren` dependency's features
set back to `["alloc", "libm"]`, which keeps the comparison with 2026-09-20 one
of code rather than of configuration; the port's default `core_full` feature,
where it exists, still selects the whole core library. `--as-shipped` leaves
the manifest alone and measures whatever the port at `--rev` declares.

**How a symbol is counted.** `llvm-nm -S` lists every symbol with a size; the
crate's are the ones whose mangled (v0) name contains the crate root
`C...4wren`. That includes Rust's own generic code *instantiated by* `wren` --
a `BTreeMap<String, usize>::insert`, a sort of `(u32, u32, u32)` -- because v0
mangling names the crate that instantiated it. The demangled name then decides
the row, by the first `wren::<module>` path in it: `<wren::core::install_num::
{closure#3} as FnOnce<(&mut wren::vm::Vm, usize)>>::call_once` is a `Num`
primitive, not interpreter code, and `<Vec<wren::value::Value>>::push` is the
value module's. A name with no `wren::` path at all is generic `core`/`alloc`
code instantiated for `wren`'s purposes, and is the monomorphisations row.

Inside `wren::core`, a closure or function under `install_<class>` is that
class's primitives -- `install_num_extras` is `Num`, `install_lazy_sequences`
is `Sequence`; `wren::core::install` itself is the binding table; anything else
in `wren::core` is a shared helper (argument checks, indexing, the map probe).

**What is not counted, and why.**

- *Everything outside the crate*: esp-hal, esp-alloc, esp-println, the port's
  own `main`, `libm`, and Rust's `core`/`alloc` code that `wren` did not
  instantiate -- among it the float formatting and parsing (Grisu, Dragon,
  `dec2flt`), which the README measures separately. A CH32 build would not link
  the ESP crates, and the rest is not `wren`'s code.
- *Anything without a symbol size*: string literals, the signature strings
  `core::install` passes, jump tables, constant pools. They are in `.rodata` as
  anonymous data, so the binding table's row is its *code*, not its strings.
- *Assembler-local labels* (`.L...`), which llvm-nm lists and GNU nm does not:
  the lookup tables LLVM builds from a `match`. Leaving them out makes the two
  tools agree; on 7e864d3 they were 72 B.

**A row is only as fine as the symbols.** What fat LTO inlines is counted
where it landed: in the `uwren` build at cac0550 `wren::core::install` has no
symbol of its own, and its bytes are inside `Vm::build_frozen`, in the
interpreter's row. And a generic instantiated over a `wren` type goes to that
type's module -- on 7e864d3, 3,264 of the 3,470 B counted as handles were a
stable sort of `(usize, ObjectId)` pairs. `--detail` shows which is which.

So the figure is the code `wren` contributes to the image, which is the part a
smaller language would have to shrink. The rows always sum to the total: a
`wren` module this tool does not know gets a row of its own rather than
disappearing.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PORT = Path("ports") / "esp32c6-wrenc-rs"
BINARY = "esp32c6-wrenc-rs"
TARGET = "riscv32imac-unknown-none-elf"

# The crate root in a v0-mangled name: `C`, an optional disambiguator
# `s<base62>_`, then the identifier with its length. A crate called
# `wrenfoo` cannot match, because the length is part of the pattern; nothing
# can be required of the character after it, since the next path element may
# open with its own disambiguator -- `wren::core` is `_4wrens_4core`.
WREN_ROOT = re.compile(r"C(?:s[0-9A-Za-z]*_)?4wren")

# The first `wren::<module>` path in a demangled name, and the item under it.
WREN_PATH = re.compile(r"wren::(\w+)(?:::(\w+))?")

# Which class an `install_*` function's primitives belong to. Prefixes are
# matched longest first, so `install_num_extras` reaches `Num` rather than
# stopping at nothing.
INSTALL_CLASS = {
    "install_num": "Num",
    "install_num_extras": "Num",
    "install_string": "String",
    "install_string_extras": "String",
    "install_string_views": "String",
    "install_list": "List",
    "install_list_extras": "List",
    "install_map": "Map",
    "install_sequence": "Sequence",
    "install_lazy_sequences": "Sequence",
    "install_range": "Range",
    "install_fiber": "Fiber",
    "install_system": "System",
    "install_object": "Object/Class",
    "install_class": "Object/Class",
    "install_metaclasses": "Object/Class",
    "install_bool": "Bool/Null",
    "install_null": "Bool/Null",
    "install_fn": "Fn",
    "install_random": "random",
    "install_meta": "meta",
}

# The machine: every module that is not the core library.
MACHINE = {
    "vm": "interpreter",
    "heap": "heap and collector",
    "handle": "handles",
    "wrenc": "loader",
    "value": "value",
    "object": "object",
    "bytecode": "bytecode",
    "symbol": "symbol",
}

# The README's rows, in its order: (label, [parts]). A part is a key that
# `classify` returns.
ROWS = [
    ("the machine", ["interpreter", "heap and collector", "handles", "loader",
                     "value", "object", "bytecode", "symbol"]),
    ("`Num`", ["Num"]),
    ("`core::install`", ["core::install"]),
    ("`String`", ["String"]),
    ("`List`, `Map`, `Sequence`", ["List", "Map", "Sequence"]),
    ("core helpers", ["core helpers"]),
    ("`random`, `meta`", ["random", "meta"]),
    ("Rust monomorphisations", ["BTreeMap", "sort", "str", "Vec and RawVec",
                                "other generic"]),
    ("`Range`, `Fiber`, `System`, `Object`/`Class`, `Bool`/`Null`, `Fn`",
     ["Range", "Fiber", "System", "Object/Class", "Bool/Null", "Fn"]),
    ("`math` -- the transcendentals' glue", ["math"]),
]


def find_nm():
    """llvm-nm from the active Rust toolchain, else the Espressif binutils one.

    Either reads the same symbol table; llvm-nm demangles v0 names without the
    crate hashes, which keeps the detail listing readable.
    """
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True,
                             text=True, check=True).stdout.strip()
    for candidate in Path(sysroot).glob("lib/rustlib/*/bin/llvm-nm"):
        return str(candidate)
    found = shutil.which("llvm-nm")
    if found:
        return found
    espressif = sorted(Path.home().glob(
        ".espressif/tools/riscv32-esp-elf/*/riscv32-esp-elf/bin/riscv32-esp-elf-nm"))
    if espressif:
        return str(espressif[-1])
    sys.exit("no nm found: install llvm-tools (rustup component add llvm-tools)")


def symbols(nm, elf):
    """Every sized symbol, as (size, mangled, demangled).

    `-p` keeps the symbol table's own order, so the mangled and the demangled
    listing line up row for row; the addresses are checked to be sure.
    """
    def listing(*flags):
        out = subprocess.run([nm, "-S", "-p", *flags, str(elf)], capture_output=True,
                             text=True, check=True).stdout
        rows = []
        for line in out.splitlines():
            parts = line.split(" ", 3)
            # A symbol without a size has three fields, not four.
            if len(parts) != 4 or not re.fullmatch(r"[0-9a-f]+", parts[1]):
                continue
            rows.append((parts[0], int(parts[1], 16), parts[2], parts[3]))
        return rows

    mangled = listing()
    demangled = listing("--demangle")
    if len(mangled) != len(demangled):
        sys.exit("the mangled and demangled listings differ in length")
    result = []
    for (address, size, kind, raw), (address2, _, _, readable) in zip(mangled, demangled):
        if address != address2:
            sys.exit(f"listings out of step at {address} / {address2}")
        # Strip the crate hashes the binutils demangler prints, `wren[e1ff...]`.
        readable = re.sub(r"\[[0-9a-f]{16}\]", "", readable)
        result.append((size, kind, raw, readable))
    return result


def classify(raw, readable):
    """Which part of the README table a `wren` symbol belongs to."""
    match = WREN_PATH.search(readable)
    if match is None:
        # Generic `core`/`alloc` code that `wren` instantiated.
        if "btree" in readable:
            return "BTreeMap"
        if "::sort::" in readable or "sort_by" in readable:
            return "sort"
        if "str::" in readable or "<str>" in readable or "char" in readable:
            return "str"
        if "raw_vec" in readable or "alloc::vec" in readable:
            return "Vec and RawVec"
        return "other generic"
    module, item = match.group(1), match.group(2) or ""
    if module == "core":
        if item == "install":
            return "core::install"
        if item in INSTALL_CLASS:
            return INSTALL_CLASS[item]
        return "core helpers"
    if module in MACHINE:
        return MACHINE[module]
    if module == "math":
        return "math"
    # A module this tool has not met: its own row, never dropped.
    return f"wren::{module}"


def classify_2026_09_20(raw, readable):
    """The rules the first measurement ran, kept to reproduce it.

    They test substrings of the *mangled* name. Two consequences, both visible
    in the 111,640 B table: `wren4core` never matches, because the module is
    mangled `s_4core` (it shares its name with the `core` crate), so every
    shared helper and `core::install` itself fell through to "other"; and
    `install_sequence` does not match `install_lazy_sequences`, whose closures
    fell there too.
    """
    n = raw
    if "install_num" in n: return "Num"
    if "install_string" in n: return "String"
    if "install_list" in n: return "List"
    if "install_map" in n: return "Map"
    if "install_range" in n: return "Range"
    if "install_fiber" in n or "2vm" in n and "fiber" in n.lower(): return "Fiber"
    if "install_fn" in n: return "Fn"
    if "install_object" in n or "install_class" in n: return "Object/Class"
    if "install_bool" in n or "install_null" in n: return "Bool/Null"
    if "install_sequence" in n: return "Sequence"
    if "install_system" in n: return "System"
    if "install_random" in n: return "random"
    if "install_meta" in n: return "meta"
    if "wren4core" in n: return "core helpers"
    if "wren2vm" in n: return "interpreter"
    if "wren4heap" in n: return "heap and collector"
    if "wren6object" in n: return "object"
    if "wren5value" in n: return "value"
    if "wren8bytecode" in n: return "bytecode"
    if "wren5wrenc" in n: return "loader"
    if "wren4math" in n: return "math"
    if "wren6symbol" in n: return "symbol"
    if "wren6handle" in n: return "handles"
    return "other"


def build(rev, as_shipped, keep):
    """Build the port at `rev` in a throwaway worktree; return (elf, cleanup).

    **The worktree's path is fixed by the commit, not random**, because the
    path moves the image. Cargo hashes a path dependency's location into the
    crate's metadata, the hash is in every symbol name, and fat LTO's inlining
    decisions follow the order those names give: 7e864d3, built from three
    different directories, came out at 111,640, 111,682 and 111,698 B -- one
    or two small generic functions inlined in one build and kept out of line
    in another; 6a143d2, from two, at 107,344 and 107,446 B, every byte of the
    difference in the monomorphisations row. A fixed path makes a commit's
    figure the same on every run; it cannot make it the same as a build from
    another checkout, so a change of up to about a hundred bytes between
    commits is not a finding.
    """
    full = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--verify", f"{rev}^{{commit}}"],
                          capture_output=True, text=True, check=True).stdout.strip()
    scratch = Path(tempfile.gettempdir()) / f"uwren-breakdown-{full[:12]}"
    if scratch.exists():
        raise SystemExit(f"{scratch} exists: a run is in progress, or one was kept "
                         f"(git worktree remove --force {scratch / 'tree'})")
    scratch.mkdir()
    tree = scratch / "tree"
    subprocess.run(["git", "-C", str(ROOT), "worktree", "add", "--detach", "--quiet",
                    str(tree), rev], check=True)

    def cleanup():
        if keep:
            print(f"kept: {scratch}", file=sys.stderr)
            return
        subprocess.run(["git", "-C", str(ROOT), "worktree", "remove", "--force",
                        str(tree)], check=False)
        shutil.rmtree(scratch, ignore_errors=True)

    try:
        manifest = tree / PORT / "Cargo.toml"
        if not as_shipped:
            text = manifest.read_text()
            # The one line that declares the `wren` dependency.
            pattern = re.compile(r'^(wren = \{ path = "[^"]+", default-features = false, '
                                 r'features = )\[[^\]]*\]', re.MULTILINE)
            text, count = pattern.subn(r'\1["alloc", "libm"]', text)
            if count != 1:
                raise SystemExit(f"could not find the wren dependency in {manifest}")
            manifest.write_text(text)
        environment = dict(os.environ)
        environment.pop("CARGO_TARGET_DIR", None)
        subprocess.run(["cargo", "build", "--quiet", "--profile", "size", "--bin", BINARY,
                        "--target-dir", str(scratch / "target")],
                       cwd=tree / PORT, env=environment, check=True)
        elf = scratch / "target" / TARGET / "size" / BINARY
        commit = subprocess.run(["git", "-C", str(tree), "rev-parse", "--short=7", "HEAD"],
                                capture_output=True, text=True, check=True).stdout.strip()
        line = re.search(r"^wren = .*$", manifest.read_text(), re.MULTILINE)
        rustc = subprocess.run(["rustc", "-V"], cwd=tree / PORT, capture_output=True,
                               text=True, check=True).stdout.strip()
        print(f"built      : {PORT} --bin {BINARY} --profile size at {commit}")
        print(f"wren       : {line.group(0) if line else '?'}")
        print(f"toolchain  : {rustc}")
        return elf, cleanup
    except BaseException:
        cleanup()
        raise


def report(elf, rules, detail):
    nm = find_nm()
    print(f"nm         : {nm}")
    classifier = classify_2026_09_20 if rules == "2026-09-20" else classify
    parts = defaultdict(int)
    members = defaultdict(list)
    for size, kind, raw, readable in symbols(nm, elf):
        if not WREN_ROOT.search(raw):
            continue
        # Assembler-local labels: llvm-nm lists the `.Lswitch.table.*` lookup
        # tables LLVM makes from a `match`, GNU nm does not. Skipping them keeps
        # the count the same whichever nm runs, and the 2026-09-20 one (GNU)
        # reproducible; on that build they were four tables, 72 B.
        if raw.startswith(".L"):
            continue
        part = classifier(raw, readable)
        parts[part] += size
        members[part].append((size, readable))
    total = sum(parts.values())

    if rules == "2026-09-20":
        # The first measurement printed its buckets, largest first.
        print()
        for part, size in sorted(parts.items(), key=lambda item: -item[1]):
            print(f"{size:>8,} B  {size * 100 / total:5.1f}%  {part}")
        print(f"{total:>8,} B  the wren crate")
    else:
        rows = list(ROWS)
        known = {part for _, row_parts in ROWS for part in row_parts}
        for part in sorted(parts):
            if part not in known:
                rows.append((f"`{part}`", [part]))
        print()
        print(f"**The crate is {total:,} B of flash.**")
        print()
        print("| | bytes | |")
        print("|---|---|---|")
        for label, row_parts in rows:
            size = sum(parts.get(part, 0) for part in row_parts)
            present = [part for part in row_parts if parts.get(part, 0)]
            if size == 0:
                continue
            if len(present) > 1:
                pieces = ", ".join(f"{part} {parts[part]:,}" for part in present)
                label = f"{label} -- {pieces}"
            print(f"| {label} | {size:,} | {size * 100 / total:.0f}% |")
        # Every part is in exactly one row, so the rows add up to the total;
        # the check is what keeps a new part from slipping out of the table.
        shown = sum(parts.get(part, 0) for _, row_parts in rows for part in row_parts)
        assert shown == total, f"rows sum to {shown:,}, not {total:,}"
        print(f"| total | **{total:,}** | |")

    if detail:
        for part, size in sorted(parts.items(), key=lambda item: -item[1]):
            print(f"\n== {part}: {size:,} B")
            for symbol_size, readable in sorted(members[part], reverse=True):
                print(f"{symbol_size:>7,}  {readable[:160]}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--rev", default="HEAD", help="the commit to build (default HEAD)")
    parser.add_argument("--elf", help="attribute this ELF instead of building one")
    parser.add_argument("--as-shipped", action="store_true",
                        help="keep the port's own wren features (on main: uwren, no floats)")
    parser.add_argument("--rules", choices=["current", "2026-09-20"], default="current",
                        help="the grouping; 2026-09-20 reproduces the first measurement")
    parser.add_argument("--detail", action="store_true", help="list every symbol by row")
    parser.add_argument("--keep", action="store_true", help="keep the worktree and target")
    arguments = parser.parse_args()

    if arguments.elf:
        report(Path(arguments.elf), arguments.rules, arguments.detail)
        return
    elf, cleanup = build(arguments.rev, arguments.as_shipped, arguments.keep)
    try:
        report(elf, arguments.rules, arguments.detail)
    finally:
        cleanup()


if __name__ == "__main__":
    main()
