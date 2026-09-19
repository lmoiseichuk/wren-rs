//! Run the benchmark programs on this implementation.
//!
//!     cargo run -p wren --release --example bench
//!
//! **The same files the C port and MicroPython were measured on** --
//! `benchmarks/wren/*.wren`, unchanged -- so the numbers can be set beside
//! `doc/wren/benchmarks.md` rather than only beside each other.
//!
//! A host run is not the comparison that counts: the published figures are
//! from an ESP32-C6, and a workstation says nothing about a part with no FPU
//! and a 160 MHz clock. What it does say is whether the programs run at all
//! and produce the right answers, which has to be true before any timing means
//! anything.
//!
//! **`--repeat N` is for optimisation work, not for publishing.** A single run
//! of these on a workstation is a few tens of milliseconds, which is inside
//! the noise of anything a machine does between two processes -- so comparing
//! one run against another cannot tell a 20% win from a busy core. Repeating
//! and taking the *fastest* run does: the minimum is the one least disturbed
//! by whatever else the machine was doing, where a mean is dragged around by
//! exactly that.

use std::time::Instant;

fn main() {
    // `--census` counts what is actually on the heap, by type. It exists to
    // decide one question with a measurement rather than an intuition: whether
    // giving each type its own table -- so a `List` costs 12 bytes rather than
    // the 24 every slot costs today -- would be worth the six free lists it
    // takes. See `doc/wren-rs/design.md`.
    let census = std::env::args().any(|argument| argument == "--census");

    let repeat: usize = std::env::args()
        .skip_while(|argument| argument != "--repeat")
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    // A bare argument names one benchmark, which is what profiling wants: the
    // four have completely different shapes and a mixed profile tells you
    // about the average of them, which is nothing.
    let only: Option<String> = std::env::args()
        .skip(1)
        .find(|argument| !argument.starts_with("--") && argument.parse::<usize>().is_err());

    let mut files: Vec<_> = std::fs::read_dir("benchmarks/wren")
        .expect("benchmarks/wren")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "wren")
        })
        .filter(|path| match &only {
            Some(name) => path.file_stem().is_some_and(|stem| stem == name.as_str()),
            None => true,
        })
        .collect();
    files.sort();

    if repeat > 1 {
        println!("best of {repeat}");
    }
    println!(
        "{:<16} {:>10} {:>12}  output",
        "benchmark", "wall (s)", "self (s)"
    );
    println!("{}", "-".repeat(64));

    for path in files {
        let source = std::fs::read_to_string(&path).expect("read");
        let name = path.file_stem().unwrap().to_string_lossy().to_string();

        // A fresh VM per run: reusing one would leave the previous run's heap
        // in place and measure the collector against a warm heap in the later
        // runs and a cold one in the first.
        let mut wall = f64::MAX;
        let mut vm = wren::Vm::new();
        let mut result = Ok(());
        for _ in 0..repeat {
            vm = wren::Vm::new();
            let started = Instant::now();
            result = vm.interpret(&source);
            wall = wall.min(started.elapsed().as_secs_f64());
            if result.is_err() {
                break;
            }
        }

        if census {
            vm.collect_garbage();
            println!("{name:<16} {wall:>10.3}");
            report_census(&vm);
            continue;
        }

        match result {
            Ok(()) => {
                let output = vm.output_str();
                // Each benchmark prints its own `elapsed:` line, which is the
                // figure the C port and MicroPython were compared on.
                let reported = output
                    .lines()
                    .find_map(|line| line.strip_prefix("elapsed: "))
                    .unwrap_or("-")
                    .to_string();
                let first = output.lines().next().unwrap_or("").to_string();
                println!("{name:<16} {wall:>10.3} {reported:>12}  {first}");
            }
            Err(error) => {
                println!(
                    "{name:<16} {:>10} {:>12}  ERROR line {}: {}",
                    "-",
                    "-",
                    error.line(),
                    error.message()
                );
            }
        }
    }
}

/// Count live objects by type, and price them two ways.
///
/// **The sizes are the 32-bit ones, measured for `riscv32imac`, not this
/// host's.** A census taken on a workstation is only interesting because the
/// object *graph* is the same shape on both; pricing it with 64-bit sizes
/// would answer a question nobody asked.
fn report_census(vm: &wren::Vm) {
    use wren::object::ObjectType;

    // One slot today, whatever the type: the largest variant sets it.
    const SLOT: usize = 24;

    // What each payload would cost in a table of its own. `Option<T>` is what
    // a free slot in such a table holds, so these include the niche or the
    // tag, exactly as `Option<Object>` does today.
    fn own_size(kind: ObjectType) -> usize {
        match kind {
            // Boxed today and still boxed: only the pointer is in the table.
            ObjectType::Class | ObjectType::Fn | ObjectType::Closure | ObjectType::Fiber => 4,
            ObjectType::Instance => 16,
            ObjectType::List => 12,
            ObjectType::Map => 16,
            ObjectType::Range => 24,
            ObjectType::String => 16,
            ObjectType::Upvalue => 16,
        }
    }

    // **What the object's own `Vec`s hold, and what they cost to hold it.**
    // Every non-empty `Vec` inside an object is a separate call to the
    // allocator, with a header and rounding of its own -- and on a part with
    // 320 KB that overhead is the number worth knowing. `HEADER` is esp-alloc's
    // per-block cost; it is an estimate and labelled as one.
    const HEADER: usize = 8;

    let mut counts: Vec<(ObjectType, usize, usize, usize, usize)> = Vec::new();
    for id in vm.heap.ids() {
        let Some(kind) = vm.heap.type_of(id) else {
            continue;
        };
        let (used, contents, blocks) = contents_detail(&vm.heap, id, kind);
        match counts.iter_mut().find(|(seen, ..)| *seen == kind) {
            Some((_, count, bytes, allocations, wanted)) => {
                *count += 1;
                *bytes += contents;
                *allocations += blocks;
                *wanted += used;
            }
            None => counts.push((kind, 1, contents, blocks, used)),
        }
    }
    counts.sort_by_key(|(_, count, ..)| std::cmp::Reverse(*count));

    println!(
        "    {:<10} {:>6} {:>9} {:>10} {:>9} {:>7}",
        "type", "live", "slots B", "contents B", "slack B", "blocks"
    );
    let (mut live, mut typed, mut contents, mut blocks) = (0usize, 0usize, 0usize, 0usize);
    let mut slack = 0usize;
    for (kind, count, bytes, allocations, wanted) in &counts {
        live += count * SLOT;
        typed += count * own_size(*kind);
        contents += bytes;
        blocks += allocations;
        slack += bytes.saturating_sub(*wanted);
        println!(
            "    {:<10} {count:>6} {:>9} {bytes:>10} {:>9} {allocations:>7}",
            format!("{kind:?}"),
            count * SLOT,
            bytes.saturating_sub(*wanted)
        );
    }
    let saved = live.saturating_sub(typed);
    let percent = (saved * 100).checked_div(live).unwrap_or(0);
    let total = live + contents + blocks * HEADER;
    println!(
        "    {:<10} {:>6} {live:>9} {contents:>10} {slack:>9} {blocks:>7}",
        "total",
        counts.iter().map(|(_, count, ..)| count).sum::<usize>()
    );
    println!(
        "    live heap {total} B = {live} slots + {contents} contents + {} allocator headers",
        blocks * HEADER
    );
    println!("    per-type tables would save {saved} B of the {live} B of slots ({percent}%)");
    report_method_tables(vm);
}

/// What an object's own allocations hold, and how many blocks they are.
///
/// Counted with `capacity`, not `len`: a `Vec` that grew and shrank is still
/// holding the memory, and the allocator has not heard otherwise.
/// The same accounting, split into what is used and what is reserved.
///
/// The gap between the two is memory the allocator has handed out and nobody
/// is using -- the thing that turned out to be 14,560 B of the method tables.
/// Worth knowing per type rather than in total, because the fix differs: a
/// class can be shrunk once and never grows again, where a list being built
/// would only buy a realloc on its next push.
fn contents_detail(
    heap: &wren::Heap,
    id: wren::ObjectId,
    kind: wren::object::ObjectType,
) -> (usize, usize, usize) {
    use wren::object::ObjectType;

    const VALUE: usize = 8;
    fn pair(used: usize, capacity: usize, element: usize) -> (usize, usize, usize) {
        match capacity {
            0 => (0, 0, 0),
            capacity => (used * element, capacity * element, 1),
        }
    }

    match kind {
        // **An instance's fields are in the heap's arena now**, not in a
        // block of its own, so they cost no allocator header and there is no
        // capacity to be slack.
        ObjectType::Instance => {
            let count = heap.instance_fields(id).len();
            (count * VALUE, count * VALUE, 0)
        }
        ObjectType::List => match heap.list(id) {
            Some(list) => pair(list.elements.len(), list.elements.capacity(), VALUE),
            None => (0, 0, 0),
        },
        ObjectType::String => match heap.string(id) {
            Some(string) => pair(string.bytes.len(), string.bytes.capacity(), 1),
            None => (0, 0, 0),
        },
        ObjectType::Map => match heap.map(id) {
            Some(map) => pair(map.entries.len(), map.entries.capacity(), 16),
            None => (0, 0, 0),
        },
        // A method table is one block; the class itself is a second, because
        // it is still boxed.
        ObjectType::Class => match heap.class(id) {
            Some(class) => {
                let (used, bytes, blocks) = pair(class.methods.len(), class.methods.capacity(), 4);
                (used + 56, bytes + 56, blocks + 1)
            }
            None => (0, 0, 0),
        },
        // A closure lives in its slot now, so its upvalue vector is its only
        // separate block.
        ObjectType::Closure => match heap.closure(id) {
            Some(closure) => pair(closure.upvalues.len(), closure.upvalues.capacity(), 4),
            None => (0, 0, 0),
        },
        ObjectType::Fn => (48, 48, 1),
        ObjectType::Fiber => match heap.fiber(id) {
            Some(fiber) => {
                // A fiber's stack reaches a high-water mark and stays there;
                // the frame vector does the same.
                let (used, bytes, blocks) = pair(fiber.stack.len(), fiber.stack.capacity(), VALUE);
                let frames_used = fiber.frames.len() * 24;
                let frames_held = fiber.frames.capacity() * 24;
                (
                    used + frames_used + 56,
                    bytes + frames_held + 56,
                    blocks + 2,
                )
            }
            None => (0, 0, 0),
        },
        ObjectType::Range | ObjectType::Upvalue => (0, 0, 0),
    }
}

/// Price the method tables three ways: as they are, paged, and sparse.
///
/// A class's table is a `Vec<Option<Method>>` indexed by *global* method
/// symbol, so it is as long as the highest symbol the class answers to and
/// almost all of it is `None`. Whether that is best fixed by paging the symbol
/// space or by hashing it depends entirely on whether a class's symbols
/// cluster, which is a fact about the program rather than something to reason
/// about -- so it is counted here.
fn report_method_tables(vm: &wren::Vm) {
    // What one entry costs today: `Option<Method>`, 8 bytes on a 32-bit part.
    const ENTRY: usize = 4;

    let mut classes = 0usize;
    let mut today = 0usize;
    let mut defined = 0usize;
    let mut longest = 0usize;
    let mut reserved = 0usize;
    // Paged: a directory of pointers, plus a full page for each page that
    // holds at least one method.
    let mut paged = [0usize; 3];
    let pages = [8usize, 16, 32];

    for id in vm.heap.ids() {
        let Some(class) = vm.heap.class(id) else {
            continue;
        };
        classes += 1;
        let length = class.methods.len();
        longest = longest.max(length);
        today += length * ENTRY;
        reserved += class.methods.capacity() * ENTRY;
        defined += class
            .methods
            .iter()
            .filter(|slot| **slot != wren::object::NO_METHOD)
            .count();

        for (which, size) in pages.iter().enumerate() {
            let directory = length.div_ceil(*size);
            let mut occupied = 0usize;
            for page in 0..directory {
                let from = page * size;
                let to = (from + size).min(length);
                if class.methods[from..to]
                    .iter()
                    .any(|slot| *slot != wren::object::NO_METHOD)
                {
                    occupied += 1;
                }
            }
            paged[which] += directory * 4 + occupied * size * ENTRY;
        }
    }

    // Sparse: an open-addressed table at 50% load, holding symbol and method.
    let sparse = {
        let mut total = 0usize;
        for id in vm.heap.ids() {
            let Some(class) = vm.heap.class(id) else {
                continue;
            };
            let count = class
                .methods
                .iter()
                .filter(|slot| **slot != wren::object::NO_METHOD)
                .count();
            let mut capacity = 8usize;
            while capacity < count * 2 {
                capacity *= 2;
            }
            // 4 bytes of symbol beside each 8-byte entry.
            total += capacity * (ENTRY + 4);
        }
        total
    };

    println!(
        "    method tables: {classes} classes, {defined} methods defined, longest table {longest}"
    );
    println!("      reserved     {reserved:>8} B  (what the Vecs actually hold)");
    println!(
        "      used         {today:>8} B  -- {} B of that is Vec slack",
        reserved.saturating_sub(today)
    );
    for (which, size) in pages.iter().enumerate() {
        let saved = today.saturating_sub(paged[which]);
        let percent = (saved * 100).checked_div(today).unwrap_or(0);
        println!(
            "      paged/{size:<3}    {:>8} B  saves {saved} B ({percent}%)",
            paged[which]
        );
    }
    let saved = today.saturating_sub(sparse);
    let percent = (saved * 100).checked_div(today).unwrap_or(0);
    println!("      sparse       {sparse:>8} B  saves {saved} B ({percent}%)");
}
