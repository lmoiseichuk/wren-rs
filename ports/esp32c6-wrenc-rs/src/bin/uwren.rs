//! µwren on an 8 KB heap: the whole question in one image.
//!
//! Integers only, no compiler, a VM carrying only the methods `fib` calls,
//! and `wren::uheap::Arena` as the allocator over a fixed 8 KB buffer — which
//! is a CH32V006's entire RAM. If this runs, the part is reachable; if it
//! does not, the number it stops at is the number to work on.
//!
//! The ESP32 stands in for the target. What it cannot stand in for is flash:
//! roughly 44 KB of this image is esp-hal, `esp-println` and ROM startup that
//! a CH32 port would not have.

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::main;
use esp_hal::time::Instant;
use esp_println::println;
use wren::uheap::{Arena, Unit};
use wren::Vm;

#[path = "../appdesc.rs"]
mod appdesc;

// **The core library, compiled into the image rather than built in RAM.**
// Generated from the same `.wrenc` this program runs:
//
//     cargo run --example freeze -- benchmarks/wrenc/fib.wrenc \
//         > ports/esp32c6-wrenc-rs/src/bin/fib_core.rs
//
// Together with the bytecode below, that puts the whole program -- classes,
// method tables, names and code -- in flash, leaving RAM for what it computes.
#[path = "fib_core.rs"]
mod fib_core;

/// The whole heap this program gets -- there is no other allocator, so this is
/// the RAM figure a board has to meet. `fib` peaks at 11,464 bytes of it (run
/// the program and read the `peak` column), so 16 KiB is the smallest round
/// size that holds it. A CH32V006 has 8,192 bytes, and that is still 3.3 KiB
/// short: see `doc/wren-rs/memory.md` for where the excess goes.
const HEAP_BYTES: usize = 16 * 1024;
const HEAP_UNITS: usize = HEAP_BYTES / core::mem::size_of::<Unit>();

#[global_allocator]
static ALLOCATOR: Arena<HEAP_UNITS> = Arena::new();

const FIB: &[u8] = include_bytes!("../../../../benchmarks/wrenc/fib.wrenc");

/// What the heap looks like at one moment.
fn report(when: &str) {
    let (used, peak, largest, blocks, refused) = ALLOCATOR.stats();
    println!(
        "{when:<14}: used {used} B, peak {peak} B, largest free {largest} B, \
         {blocks} blocks, {refused} refused"
    );
}

/// What the VM is holding, line by line, and whether it adds up.
///
/// **Two independent accountings, printed together.** The VM says what it
/// asked for; the arena says what it actually cut. They will not match to the
/// byte -- an allocator rounds, and `Vec::capacity` is what a vector was given
/// rather than what the allocator charged for it -- but a gap much wider than
/// the block count explains means something is being paid for that no line
/// here names.
#[cfg(feature = "census")]
fn breakdown(vm: &Vm, baseline: usize) {
    // **The walk comes first, before anything below allocates.** Both census
    // calls build vectors, and they build them in the arena being measured --
    // so asking the arena afterwards would count the question as part of the
    // answer.
    let mut live = 0;
    let mut holes = 0;
    let mut live_blocks = 0;
    let mut hole_blocks = 0;
    // Buckets are powers of two: 1-16 B, 17-32, and so on to 1 KB and up.
    let mut buckets = [0usize; 8];
    ALLOCATOR.walk(|bytes, free| {
        match free {
            true => {
                holes += bytes;
                hole_blocks += 1;
            }
            false => {
                live += bytes;
                live_blocks += 1;
                let bucket = match bytes {
                    0..=16 => 0,
                    17..=32 => 1,
                    33..=64 => 2,
                    65..=128 => 3,
                    129..=256 => 4,
                    257..=512 => 5,
                    513..=1024 => 6,
                    _ => 7,
                };
                buckets[bucket] += 1;
            }
        }
    });

    let mut asked = 0;

    println!();
    println!("what the VM holds, and why                      bytes");
    println!("-----------------------------------------------------");

    // **An object's cost is its slot plus its contents, and the slot is
    // already in the table's capacity below.** Charging `Heap::bytes` whole
    // would count every occupied slot twice, so the slots come back out here
    // and what is left is what the objects themselves own: a string's text,
    // a class's method table.
    let mut occupied = 0;
    for (_, slots, free, size) in vm.heap.slot_census() {
        occupied += (slots - free) * size;
    }
    // Live objects per table, so the slot lines below can say how much of the
    // capacity is actually holding something.
    let live_slots = vm.heap.slot_census();
    let contents = vm.heap.bytes().saturating_sub(occupied);
    println!("  object contents (strings, method tables) {contents:>8}");
    for (kind, count, bytes) in vm.heap.contents_census() {
        if bytes == 0 {
            continue;
        }
        println!("    {kind:<18} {count:>3} x            {bytes:>6}");
    }
    asked += contents;

    // Slot vectors and the bookkeeping sized from them. An empty table costs
    // nothing, so only the ones that grew are worth a line.
    let mut slots = 0;
    let mut books = 0;
    for (name, table, bookkeeping) in vm.heap.memory_census() {
        if table + bookkeeping == 0 {
            continue;
        }
        let held = live_slots
            .iter()
            .find(|(kind, ..)| *kind == name)
            .map(|(_, slots, free, _)| slots - free)
            .unwrap_or(0);
        let room = live_slots
            .iter()
            .find(|(kind, ..)| *kind == name)
            .map(|(_, slots, ..)| *slots)
            .unwrap_or(0);
        println!(
            "  {name:<12} slots {table:>6} + {bookkeeping:>3} books   {held} of {room} slots used"
        );
        slots += table;
        books += bookkeeping;
    }
    asked += slots + books;

    println!("-----------------------------------------------------");
    let mut on_the_stack = 0;
    for (name, bytes, used, room) in vm.memory_census() {
        if bytes == 0 {
            continue;
        }
        // **The `Vm` is a local in `main`, so it is stack and not arena.**
        // Real RAM either way and worth seeing, but it must not be added to a
        // total the allocator is going to be asked to confirm.
        if name == "Vm struct" {
            println!("  {name:<20} {bytes:>8}                (stack, not heap)");
            on_the_stack = bytes;
            continue;
        }
        match room {
            0 => println!("  {name:<20} {bytes:>8}"),
            _ => println!("  {name:<20} {bytes:>8}   {used} of {room} used"),
        }
        asked += bytes;
    }

    println!("-----------------------------------------------------");
    println!("  the VM says it asked the arena for     {asked:>10}");
    println!("  ...and holds this much stack besides   {on_the_stack:>10}");

    // **A closed ledger, not a list of numbers.** Every line below is
    // measured, and they are printed in an order that adds up to the arena's
    // own total -- so a term nobody thought of shows up as a discrepancy
    // rather than hiding inside a plausible-looking figure.
    let requested = ALLOCATOR.requested();
    let records = (live_blocks + hole_blocks) * core::mem::size_of::<Unit>();
    // The two census calls above built vectors, and they built them here.
    let census_itself = requested.saturating_sub(asked + baseline);

    println!();
    println!("  reconciling, in bytes:");
    println!("    the VM's lines above                 {asked:>10}");
    println!("    the runtime, before the VM existed   {baseline:>10}");
    println!("    this census's own vectors            {census_itself:>10}");
    println!("    -------------------------------------------");
    println!("    every caller asked for               {requested:>10}");
    let (reuse, cut) = ALLOCATOR.wasted();
    println!(
        "    the arena's overhead                 {:>10}",
        (live + holes).saturating_sub(requested)
    );
    // **Cumulative, so these can exceed the line above.** Both are decisions
    // taken when a block is cut and a later free does not give them back --
    // which is the point: they say which rule to change, not what is held now.
    println!("      ...holes reused whole, ever        {reuse:>10}");
    println!("      ...alignment gaps absorbed, ever   {cut:>10}");
    println!(
        "    block records                        {records:>10}   {} blocks x {} B",
        live_blocks + hole_blocks,
        core::mem::size_of::<Unit>()
    );
    println!("    -------------------------------------------");
    println!(
        "    the arena holds                      {:>10}   of which {holes} B is holes",
        live + holes + records
    );

    println!();
    println!("  block sizes:");
    let labels = ["<=16 B", "<=32 B", "<=64 B", "<=128 B", "<=256 B", "<=512 B", "<=1 KB", "> 1 KB"];
    for (label, count) in labels.iter().zip(buckets.iter()) {
        if *count > 0 {
            println!("    {label:<10} {count:>4}");
        }
    }
}

#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());

    println!();
    println!("uwren -- integers only, no compiler, uheap");
    println!("heap        : {} B fixed", ALLOCATOR.capacity());

    let manifest = match wren::wrenc::manifest(FIB) {
        Ok(manifest) => manifest,
        Err(error) => {
            println!("manifest    : FAILED -- {}", error.message());
            loop {}
        }
    };
    println!("fib asks for: {} core methods", manifest.signatures.len());

    // Taken before the VM is built, because the arena is the global allocator
    // and the runtime is already in it -- see `breakdown`.
    let baseline = ALLOCATOR.stats().0;
    report("before the VM");

    let mut vm = Vm::with_frozen_core(&fib_core::CORE, &manifest);
    // **A stale generated core dispatches to the wrong method and says
    // nothing**, so the one check worth its bytes is that its symbols are the
    // ones this build interns. See `FrozenCore::disagreement`.
    if let Some(why) = fib_core::CORE.disagreement(&vm.method_names, vm.primitives.len()) {
        println!("frozen core : STALE -- {why}");
        loop {}
    }
    report("after the VM");
    #[cfg(feature = "census")]
    breakdown(&vm, baseline);

    let origin = Instant::now();
    vm.set_clock(move || origin.elapsed().as_micros() as f64 / 1_000_000.0);

    let loaded = match wren::wrenc::load(&mut vm, FIB) {
        Ok(loaded) => loaded,
        Err(error) => {
            println!("load        : FAILED -- {}", error.message());
            loop {}
        }
    };
    report("after load");

    let started = Instant::now();
    let outcome = vm.run_closure(loaded.closure);
    let wall = started.elapsed();
    let peak = vm.heap.peak_bytes();

    match outcome {
        Ok(()) => {
            let output = vm.output_str();
            let answer = output.lines().find(|line| !line.starts_with("elapsed:"));
            println!("fib         : {}", answer.unwrap_or("(no output)"));
            println!("wall        : {} us", wall.as_micros());
        }
        Err(error) => println!("fib         : ERROR line {}: {}", error.line, error.message),
    }
    report("after the run");
    println!("object peak : {peak} B");
    println!();
    println!("done");
    loop {}
}
