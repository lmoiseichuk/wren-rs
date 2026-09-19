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

    let mut counts: Vec<(ObjectType, usize, usize, usize)> = Vec::new();
    for id in vm.heap.ids() {
        let Some(object) = vm.heap.get(id) else {
            continue;
        };
        let kind = object.object_type();
        let (contents, blocks) = contents_of(object);
        match counts.iter_mut().find(|(seen, ..)| *seen == kind) {
            Some((_, count, bytes, allocations)) => {
                *count += 1;
                *bytes += contents;
                *allocations += blocks;
            }
            None => counts.push((kind, 1, contents, blocks)),
        }
    }
    counts.sort_by_key(|(_, count, ..)| std::cmp::Reverse(*count));

    println!(
        "    {:<10} {:>6} {:>9} {:>9} {:>10} {:>7} {:>9}",
        "type", "live", "slots B", "own B", "contents B", "blocks", "header B"
    );
    let (mut live, mut typed, mut contents, mut blocks) = (0usize, 0usize, 0usize, 0usize);
    for (kind, count, bytes, allocations) in &counts {
        live += count * SLOT;
        typed += count * own_size(*kind);
        contents += bytes;
        blocks += allocations;
        println!(
            "    {:<10} {count:>6} {:>9} {:>9} {bytes:>10} {allocations:>7} {:>9}",
            format!("{kind:?}"),
            count * SLOT,
            count * own_size(*kind),
            allocations * HEADER
        );
    }
    let saved = live.saturating_sub(typed);
    let percent = (saved * 100).checked_div(live).unwrap_or(0);
    let total = live + contents + blocks * HEADER;
    println!(
        "    {:<10} {:>6} {live:>9} {typed:>9} {blocks:>7} {:>9}",
        "total",
        counts.iter().map(|(_, count, ..)| count).sum::<usize>(),
        blocks * HEADER
    );
    println!(
        "    live heap {total} B = {live} slots + {contents} contents + {} allocator headers",
        blocks * HEADER
    );
    println!("    per-type tables would save {saved} B of the {live} B of slots ({percent}%)");
}

/// What an object's own allocations hold, and how many blocks they are.
///
/// Counted with `capacity`, not `len`: a `Vec` that grew and shrank is still
/// holding the memory, and the allocator has not heard otherwise.
fn contents_of(object: &wren::Object) -> (usize, usize) {
    use wren::Object;

    const VALUE: usize = 8;
    fn block(capacity: usize, element: usize) -> (usize, usize) {
        match capacity {
            0 => (0, 0),
            capacity => (capacity * element, 1),
        }
    }

    match object {
        Object::Instance(instance) => block(instance.fields.capacity(), VALUE),
        Object::List(list) => block(list.elements.capacity(), VALUE),
        Object::String(string) => block(string.bytes.capacity(), 1),
        // A method table is one block; the class itself is a second, because
        // it is boxed.
        Object::Class(class) => {
            let (bytes, blocks) = block(class.methods.capacity(), 8);
            (bytes + 40, blocks + 1)
        }
        Object::Map(map) => block(map.entries.capacity(), 16),
        Object::Closure(closure) => {
            let (bytes, blocks) = block(closure.upvalues.capacity(), 4);
            (bytes + 8, blocks + 1)
        }
        Object::Fn(_) => (28, 1),
        Object::Fiber(fiber) => {
            let (bytes, blocks) = block(fiber.stack.capacity(), VALUE);
            (bytes + 40, blocks + 1)
        }
        Object::Range(_) | Object::Upvalue(_) => (0, 0),
    }
}
