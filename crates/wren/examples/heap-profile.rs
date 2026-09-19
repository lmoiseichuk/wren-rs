//! Profile the object population and what the collector costs, over every
//! program in the repository.
//!
//!     cargo run -p wren --release --features profile --example heap-profile
//!
//! **This exists to choose what replaces mark-sweep**, and the choice needs
//! four numbers that cannot be reasoned out from the source:
//!
//! 1. **What is allocated** — counts and bytes by type. A size-class or arena
//!    allocator is worth building only if the population is concentrated.
//! 2. **What tracing costs** — the mark phase is proportional to the *live*
//!    set and is paid at every collection whether anything died or not; the
//!    sweep is proportional to the whole table, which is a different curve.
//! 3. **How much dies young** — the survival rate across collections is the
//!    whole case for a generational collector. If almost everything dies, a
//!    nursery is most of the win for a fraction of the work.
//! 4. **How much garbage is cyclic** — simulated at every collection by
//!    building reference counts over the garbage and seeing what falls to
//!    zero. This is what decides whether refcounting can stand alone, and it
//!    is the one number people usually assert rather than measure.
//!
//! Every Wren file in the repository is run: upstream's suite, which is the
//! broadest population available, plus the benchmarks and `programs/`. Files
//! that fail to compile still count -- they exercise the compiler's own
//! allocation, which is part of what a firmware pays for.

use std::path::{Path, PathBuf};
use std::time::Instant;

/// The types, in `ObjectType`'s declaration order, which is the order the
/// profile's `allocated` array is indexed by.
const TYPES: [&str; 10] = [
    "Class", "Closure", "Fn", "Instance", "List", "Map", "Range", "String", "Upvalue", "Fiber",
];

/// What one slot cost when every object shared one table: the largest variant.
/// Kept as the baseline the per-type columns are read against.
const SLOT: u64 = 24;

/// What a slot costs now, per type, on `riscv32imac`.
///
/// **Measured, not reasoned about** -- these are `size_of::<Option<T>>()` read
/// back from a cross-compile. `Class`, `Fn` and `Fiber` are 4 because they are
/// still boxed and the slot holds only the pointer; every other type's slot is
/// exactly its payload, because each has a spare bit pattern for `Option` to
/// put its discriminant in.
///
/// `Upvalue` was the exception at 24 for a 16-byte payload -- a `usize` and a
/// `Value` offer no niche between them -- which mattered because upvalues are
/// 19.2% of everything allocated. Biasing its stack slot by one makes the
/// field `NonZeroU32`, and that is the niche.
const OWN: [u64; 10] = [4, 16, 4, 16, 12, 16, 24, 16, 16, 4];

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let only = arguments
        .iter()
        .find(|argument| !argument.starts_with("--"))
        .cloned();
    // **Upstream's two GC stress tests allocate more than the other 873 files
    // put together**, so every aggregate is really a report about them unless
    // they are set aside. `--typical` does that; the top-allocators table
    // below is there so the skew is visible either way.
    let typical = arguments.iter().any(|argument| argument == "--typical");
    const STRESS: [&str; 2] = ["many_reallocations", "deeply_nested_gc"];

    let mut files = Vec::new();
    collect_wren(Path::new("vendor/wren/test"), &mut files);
    collect_wren(Path::new("benchmarks/wren"), &mut files);
    collect_wren(Path::new("programs"), &mut files);
    files.sort();
    if let Some(filter) = &only {
        files.retain(|path| path.to_string_lossy().contains(filter.as_str()));
    }
    if typical {
        files.retain(|path| {
            let name = path.to_string_lossy().to_string();
            !STRESS.iter().any(|stress| name.contains(stress))
        });
    }

    self_check();

    let mut total = wren::heap::Profile::default();
    let mut ran = 0usize;
    let mut collected_naturally = 0usize;
    let mut per_program: Vec<(u64, String)> = Vec::new();
    // type -> (disagreements, too low, too high)
    let mut mismatches: std::collections::BTreeMap<&str, (usize, usize, usize, String)> =
        std::collections::BTreeMap::new();
    let started = Instant::now();

    for path in &files {
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };

        let mut vm = wren::Vm::new();
        // The result is not interesting here: a test that fails still
        // allocated everything it allocated on the way to failing, and error
        // tests are a third of the suite.
        let _ = vm.interpret(&source);

        // **Before anything is collected**, check every reference count
        // against the references that actually exist. A missing write barrier
        // shows up here as a count that is too low, which is the one that
        // would be a use-after-free once prompt freeing is on.
        for (old, young) in match wren::Heap::nursery() {
            true => vm.heap.verify_remembered(),
            false => Vec::new(),
        } {
            let kind = vm
                .heap
                .kind_of(old)
                .map_or("?", |kind| TYPES[kind as usize]);
            let entry = mismatches
                .entry(kind)
                .or_insert((0usize, 0usize, 0usize, String::new()));
            entry.0 += 1;
            entry.1 += 1;
            if entry.3.is_empty() {
                let points_at = vm
                    .heap
                    .kind_of(young)
                    .map_or("?", |kind| TYPES[kind as usize]);
                entry.3 = format!("{} (points at a young {points_at})", path.display());
            }
        }

        let natural = vm.heap.profile().collections;
        // **One forced collection at the end**, so a program too small to
        // reach the threshold still contributes garbage to the cyclic
        // analysis. Most of the suite is that small, and excluding it would
        // leave the cyclic figure resting on four benchmarks.
        vm.collect_garbage();

        let profile = vm.heap.profile();
        if natural > 0 {
            collected_naturally += 1;
        }
        per_program.push((profile.allocated.iter().sum(), path.display().to_string()));
        add(&mut total, profile);
        ran += 1;
    }

    let wall = started.elapsed();
    report(&total, ran, collected_naturally, wall.as_nanos() as u64);

    println!();
    println!("write barriers");
    match (wren::Heap::nursery(), mismatches.is_empty()) {
        (false, _) => println!("  no young generation in this build -- try --features nursery"),
        (true, true) => {
            println!("  every old object pointing at a young one is in the remembered set")
        }
        (true, false) => {
            println!("  {:<10} {:>10}  first seen in", "type", "unrecorded");
            for (kind, (total, _, _, where_)) in &mismatches {
                println!("  {kind:<10} {total:>10}  {where_}");
            }
            println!("  an unrecorded old-to-young reference is freed while still in use");
        }
    }

    println!();
    println!("where the allocations came from");
    per_program.sort_by_key(|(count, _)| core::cmp::Reverse(*count));
    let allocations: u64 = total.allocated.iter().sum();
    for (count, path) in per_program.iter().take(8) {
        println!(
            "  {count:>12} {:>6.1}%  {path}",
            percent(*count, allocations)
        );
    }
    let rest: u64 = per_program.iter().skip(8).map(|(count, _)| count).sum();
    println!(
        "  {rest:>12} {:>6.1}%  the other {} programs",
        percent(rest, allocations),
        per_program.len().saturating_sub(8)
    );
}

fn collect_wren(root: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Upstream keeps its own benchmarks under `test/benchmark`; those
            // are desktop-sized and would dominate every figure here.
            if path.file_name().is_some_and(|name| name == "benchmark") {
                continue;
            }
            collect_wren(&path, into);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "wren")
        {
            into.push(path);
        }
    }
}

fn add(total: &mut wren::heap::Profile, one: &wren::heap::Profile) {
    for (sum, count) in total.allocated.iter_mut().zip(one.allocated.iter()) {
        *sum += count;
    }
    total.allocated_bytes += one.allocated_bytes;
    total.collections += one.collections;
    total.marked += one.marked;
    total.swept += one.swept;
    total.survived += one.survived;
    total.live_before += one.live_before;
    total.garbage_acyclic += one.garbage_acyclic;
    total.garbage_cyclic += one.garbage_cyclic;
    total.collect_nanos += one.collect_nanos;
    total.slots_swept += one.slots_swept;
    total.freed_promptly += one.freed_promptly;
    total.young_allocated += one.young_allocated;
    total.minor_collections += one.minor_collections;
    total.promoted += one.promoted;
    total.freed_young += one.freed_young;
    total.young_survived += one.young_survived;
    total.flushes += one.flushes;
    total.peak_live = total.peak_live.max(one.peak_live);
    total.peak_bytes = total.peak_bytes.max(one.peak_bytes);
}

fn report(total: &wren::heap::Profile, ran: usize, natural: usize, wall: u64) {
    let allocations: u64 = total.allocated.iter().sum();

    println!("{ran} programs, {natural} of which collected without being asked");
    println!();

    println!("population");
    println!(
        "  {:<10} {:>12} {:>7}  {:>14} {:>14}",
        "type", "allocated", "share", "one-table B", "per-type B"
    );
    let mut order: Vec<usize> = (0..TYPES.len()).collect();
    order.sort_by_key(|index| core::cmp::Reverse(total.allocated[*index]));
    let (mut slot_bytes, mut own_bytes) = (0u64, 0u64);
    for index in order {
        let count = total.allocated[index];
        if count == 0 {
            continue;
        }
        slot_bytes += count * SLOT;
        own_bytes += count * OWN[index];
        println!(
            "  {:<10} {count:>12} {:>6.1}%  {:>14} {:>14}",
            TYPES[index],
            percent(count, allocations),
            count * SLOT,
            count * OWN[index],
        );
    }
    println!(
        "  {:<10} {allocations:>12} {:>7}  {slot_bytes:>14} {own_bytes:>14}",
        "total", ""
    );
    println!(
        "  per-type tables save {} B of slots ({:.1}%) -- built, see design.md",
        slot_bytes - own_bytes,
        percent(slot_bytes - own_bytes, slot_bytes)
    );
    println!();

    println!("what the collector cost");
    println!("  collections            {:>12}", total.collections);
    println!("  objects marked         {:>12}", total.marked);
    println!("  slots swept            {:>12}", total.slots_swept);
    println!("  objects freed          {:>12}", total.swept);
    println!(
        "  time in collect        {:>12} ms  ({:.1}% of {:.0} ms total)",
        total.collect_nanos / 1_000_000,
        percent(total.collect_nanos, wall),
        wall as f64 / 1e6,
    );
    // Marking is the part that scales with the live set, and it is the part a
    // nursery or a refcount removes. Sweeping scales with the table.
    println!(
        "  marked per object freed {:>11.2}   -- tracing work per byte reclaimed",
        ratio(total.marked, total.swept)
    );
    println!();

    println!("the young generation");
    println!("  minor collections      {:>12}", total.minor_collections);
    println!("  freed young            {:>12}", total.freed_young);
    println!("  promoted               {:>12}", total.promoted);
    println!("  major collections      {:>12}", total.collections);
    println!();

    println!("do objects die young");
    println!("  allocated              {:>12}", total.young_allocated);
    println!(
        "  of those, still alive at the next collection: {:>10}  ({:.1}%)",
        total.young_survived,
        percent(total.young_survived, total.young_allocated)
    );
    // **The generational hypothesis, stated as a number.** A nursery is worth
    // building when this is low: a minor collection then traces a small live
    // set and reclaims nearly everything, and the long-lived objects it
    // promotes stop being re-marked at every later collection.
    println!(
        "  a nursery would reclaim {:>11.1}%  of everything allocated, tracing only the rest",
        100.0 - percent(total.young_survived, total.young_allocated)
    );
    println!();

    println!("how much is re-traced");
    println!("  live before collection {:>12}", total.live_before);
    println!("  survived               {:>12}", total.survived);
    // **High is the bad number here, and it is the one people expect to be
    // low.** Everything that survives a collection is marked again at the
    // next one, and again after that, for as long as it lives. A generational
    // collector exists to stop paying that; `marked per object freed` above
    // is the same waste expressed as a ratio.
    println!(
        "  survival rate          {:>11.1}%  -- survivors are re-marked at every later collection",
        percent(total.survived, total.live_before)
    );
    println!();

    println!("what the reference counts reclaimed");
    println!("  freed without a trace   {:>12}", total.freed_promptly);
    println!("  root scans to do it     {:>12}", total.flushes);
    println!(
        "  share of all reclaims  {:>11.1}%",
        percent(total.freed_promptly, total.freed_promptly + total.swept)
    );
    println!();

    println!("how much garbage is cyclic");
    let garbage = total.garbage_acyclic + total.garbage_cyclic;
    println!("  garbage seen           {:>12}", garbage);
    println!(
        "  a refcount would free  {:>12}  ({:.1}%)",
        total.garbage_acyclic,
        percent(total.garbage_acyclic, garbage)
    );
    println!(
        "  a refcount would leak  {:>12}  ({:.1}%)  -- needs a tracing backstop",
        total.garbage_cyclic,
        percent(total.garbage_cyclic, garbage)
    );
}

fn percent(part: u64, whole: u64) -> f64 {
    match whole {
        0 => 0.0,
        whole => part as f64 * 100.0 / whole as f64,
    }
}

fn ratio(top: u64, bottom: u64) -> f64 {
    match bottom {
        0 => 0.0,
        bottom => top as f64 / bottom as f64,
    }
}

/// Prove the cyclic-garbage simulation can actually see a cycle.
///
/// **A measurement that reports zero is worth nothing until it has been shown
/// to report non-zero**, and "no garbage in 873 programs was cyclic" is
/// exactly the sort of result that is one bug away from meaningless. So a
/// program that builds cycles on purpose runs first, and its answer is printed
/// beside the real one.
///
/// Two shapes, because they fail differently: a pair of objects referring to
/// each other, and one referring to itself. A simulation that forgot to count
/// self-references would still get the pair right.
fn self_check() {
    const CYCLES: &str = r#"
class Node {
  construct new() {}
  next=(value) { _next = value }
}
for (i in 1..100) {
  var a = Node.new()
  var b = Node.new()
  a.next = b
  b.next = a
}
var loops = []
for (i in 1..50) {
  var self = Node.new()
  self.next = self
}
"#;

    let mut vm = wren::Vm::new();
    let _ = vm.interpret(CYCLES);
    vm.collect_garbage();
    let profile = vm.heap.profile();
    let cyclic = profile.garbage_cyclic;
    let acyclic = profile.garbage_acyclic;

    // 100 pairs plus 50 self-loops is 250 objects that no reference count can
    // reclaim. Anything well short of that means the simulation is blind.
    let verdict = match cyclic >= 250 {
        true => "ok",
        false => "SUSPECT -- the simulation is not seeing cycles",
    };
    println!(
        "self-check: a program built to make 250 cyclic objects reports \
         {cyclic} cyclic, {acyclic} acyclic -- {verdict}"
    );
    println!();
}
