//! The Rust Wren VM on an ESP32-C6, running the benchmark set.
//!
//! **The same four programs the C port and MicroPython were measured on**, from
//! `benchmarks/wren/`, compiled into the image with `include_str!` so the
//! device needs no filesystem and no console protocol to be handed its work.
//! That also removes the transfer from the measurement: what is timed is
//! compiling and running the program, not sending it.
//!
//! Each benchmark times itself with `System.clock` and prints its own
//! `elapsed:` line, exactly as it does under the C port, so the two figures
//! answer the same question. The port adds what the program cannot see: the
//! device's own wall clock, and the heap the run consumed.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use esp_backtrace as _;
use esp_hal::main;
use esp_hal::time::{Duration, Instant};
use esp_println::println;
use wren::heap::Heap;
use wren::Vm;

// The image header the chip's bootloader looks for.
mod appdesc;
use appdesc as _;

mod counters;

/// The heap the VM allocates from.
///
/// **256 KB**, chosen to leave a program roughly what the C port left one.
/// That port had about 310 KB free at boot and its VM took 83,036 B of it,
/// leaving ~227 KB; matching the space available to the *program* is what
/// makes a heap figure comparable, rather than matching the total.
const HEAP_BYTES: usize = 320 * 1024;

/// How much garbage the collector may leave lying before it runs again.
///
/// `None` uses the growth ratio, which is what the C port is measured against
/// and therefore what the published numbers use. `Some(16 * 1024)` measured
/// `binary_trees` at 109,004 B peak instead of 117,384 -- 7% less memory for
/// 11% more time, and a peak that is the live set plus a constant rather than
/// half as much again as the live set.
const HEADROOM: Option<usize> = None;

/// How many slots one block of a slot table holds.
///
/// **The best size is a property of the program**, which is why the library
/// takes it at start-up rather than fixing it: a program that allocates a lot
/// wants a large block, because then the tail of a part-filled one is a
/// rounding error and what is saved is a pointer per block instead of one per
/// slot; a program that allocates almost nothing wants a small one, because
/// that tail is most of what it holds and there are ten tables paying it.
/// `doc/wren-rs/memory.md` carries the measurements.
///
/// `None` leaves the library's default. Set `WREN_SLOT_BLOCK` at build time
/// to sweep it -- `build.rs` is what makes the sweep actually rebuild.
const SLOT_BLOCK: Option<usize> = match option_env!("WREN_SLOT_BLOCK") {
    Some(text) => Some(decimal(text)),
    None => None,
};

/// `usize::from_str_radix` is not a const function, and this has to be one.
///
/// A build-time constant cannot be parsed by the standard library at compile
/// time, so the four lines are written out. Anything that is not a decimal
/// number stops the build here rather than becoming a wrong block size --
/// `set_slot_block` refuses a nonsense value anyway, but refusing it in the
/// compiler is better than refusing it on the device.
const fn decimal(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut value = 0;
    let mut index = 0;
    while index < bytes.len() {
        assert!(
            bytes[index] >= b'0' && bytes[index] <= b'9',
            "WREN_SLOT_BLOCK must be a decimal number"
        );
        value = value * 10 + (bytes[index] - b'0') as usize;
        index += 1;
    }
    value
}

/// The benchmarks, in the order the published table lists them, with how many
/// times each is run.
///
/// **A repeat is not the same as a longer benchmark.** The constants in
/// `benchmarks/wren/` are fixed by `benchmarks/README.md` -- the Wren and
/// Python versions must match, and every published table was measured at
/// those values -- so the programs are left exactly as they are and run more
/// than once instead. What that buys is dilution: `method_call` finishes in
/// under two seconds, of which compiling it is a fixed slice, and a change
/// worth two per cent of dispatch is hard to see underneath that. Running one
/// *compiled* chunk ten times pays the compile once and the dispatch ten
/// times. Ten compiles would dilute nothing.
///
/// **Only the two dispatch benchmarks repeat.** `method_call` is under two
/// seconds and can afford ten; `fib` is thirteen and five of those is already
/// a minute. Both hold almost nothing -- 7,484 B and 3,476 B -- so running
/// them again in the same VM costs nothing but time.
///
/// The two allocation benchmarks stay at one, and not only because they are
/// long. A second run in the same VM overlaps two live sets: `list_build`
/// overwrites its module variable with a fresh list on its first statement,
/// but the previous run's 80 KB stays reachable until that store completes,
/// and the new list's doubling transient lands on top of it -- 227 KB free
/// does not cover both, and it panics. Repeating an allocation benchmark
/// measures something other than the benchmark.
const BENCHMARKS: &[(&str, &str, u32)] = &[
    ("binary_trees", include_str!("../../../benchmarks/wren/binary_trees.wren"), 1),
    ("fib", include_str!("../../../benchmarks/wren/fib.wren"), 5),
    ("list_build", include_str!("../../../benchmarks/wren/list_build.wren"), 1),
    ("method_call", include_str!("../../../benchmarks/wren/method_call.wren"), 10),
];

/// Run only the benchmarks named here, comma separated.
///
/// **For the optimisation loop, not for a published figure.** Trying six
/// variants of one interpreter arm against `method_call` is a twenty-second
/// round trip; the whole set is a minute and a half of it is `fib`. Unset
/// runs everything, which is what a number that goes in a table needs.
const ONLY: Option<&str> = option_env!("WREN_ONLY");

/// Whether this benchmark is one of the ones asked for.
fn selected(name: &str) -> bool {
    match ONLY {
        None => true,
        Some(list) => list.split(',').any(|wanted| wanted.trim() == name),
    }
}

/// Override every repeat count, for a quick iteration.
///
/// The full set at the counts above is minutes per flash, which is right for
/// a figure that goes in a table and wrong for trying six variants of one
/// interpreter arm. `WREN_REPEATS=1` gives the short loop back.
const REPEAT_OVERRIDE: Option<u32> = match option_env!("WREN_REPEATS") {
    Some(text) => Some(decimal(text) as u32),
    None => None,
};

#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    esp_alloc::heap_allocator!(size: HEAP_BYTES);

    println!();
    println!("wren-rs on ESP32-C6");
    println!("profile      : {}", if cfg!(debug_assertions) { "debug" } else { "release" });
    println!("heap         : {HEAP_BYTES} B");
    match fresh_vm().heap.slot_block() {
        Some(slots) => println!("slot block   : {slots} slots"),
        None => println!("slot block   : flat tables"),
    }
    println!();

    // What one VM costs before it has run anything, which is the figure to set
    // beside the C port's 83,036 B resident.
    let before_vm = esp_alloc::HEAP.free();
    let probe = fresh_vm();
    let resident = before_vm.saturating_sub(esp_alloc::HEAP.free());
    drop(probe);
    println!("[vm] resident {resident} B");
    println!();

    for (name, source, repeats) in BENCHMARKS {
        if !selected(name) {
            continue;
        }
        run_one(name, source, REPEAT_OVERRIDE.unwrap_or(*repeats).max(1));
    }

    println!();
    println!("done");
    loop {}
}

/// A VM whose heap is set up the way this build asked for.
///
/// **The block size has to be chosen before the heap holds anything**, and
/// building a VM fills it -- the core classes and their names are the first
/// two dozen objects in any program. So the heap is made here, set, and handed
/// over; reaching for `vm.heap` afterwards would be too late and the setter
/// would refuse.
fn fresh_vm() -> Vm {
    let mut heap = Heap::new();
    if let Some(slots) = SLOT_BLOCK {
        assert!(
            heap.set_slot_block(slots),
            "WREN_SLOT_BLOCK must be a power of two, and the build needs the blocked-slots feature"
        );
    }
    Vm::with_heap(heap)
}

/// Compile one benchmark once, run it `repeats` times, and report what it cost.
fn run_one(name: &str, source: &str, repeats: u32) {
    // A fresh VM per benchmark, so one program's garbage is never another's
    // starting condition -- the C port restarts the board between runs for the
    // same reason.
    let mut vm = fresh_vm();
    // **A fixed heap wants a fixed ceiling on garbage.** The default growth
    // factor is upstream's 1.5x, which lets a program hold half as much
    // garbage again as it is using -- a ratio, on a part whose total is a
    // constant. See `Heap::set_headroom`.
    vm.heap.set_headroom(HEADROOM);

    // `System.clock` is a host hook on a bare-metal target: there is no
    // process clock to default to, so the port supplies the one the chip has.
    let origin = Instant::now();
    vm.set_clock(move || origin.elapsed().as_micros() as f64 / 1_000_000.0);

    let free_before = esp_alloc::HEAP.free();

    // **Compiled once, outside the measured region.** The repeat exists to
    // make the fixed cost of compiling small against the work being measured,
    // which only happens if the compile is not repeated with it.
    let chunk = match wren::compiler::compile(&mut vm, source) {
        Ok(chunk) => alloc::rc::Rc::new(chunk),
        Err(error) => {
            println!("{name:<14} COMPILE ERROR line {}: {}", error.line, error.message);
            return;
        }
    };

    // **The counter is 32 bits, and that is what bounds the repeat counts.**
    // One window covers every repeat, so the ceiling is 4,294,967,295
    // instructions for the whole set of them: `fib` at 766M a run and five
    // runs is 3.83e9, which is 89% of the range and the tightest of the four.
    // A repeat count raised past that would wrap silently and report a number
    // smaller than a single run -- so the counts in `BENCHMARKS` are not free
    // to grow without checking this.
    counters::start(counters::INSTRUCTIONS);
    let started = Instant::now();
    let mut outcome = Ok(());
    // The heap figure is taken after the *first* run, so it keeps meaning what
    // it meant before repeats existed: what one run of this program leaves
    // resident. Later runs overwrite the module's variables and make the
    // previous run's objects garbage, which is a different measurement.
    let mut free_after = free_before;
    for round in 0..repeats {
        outcome = vm.run(chunk.clone());
        if round == 0 {
            free_after = esp_alloc::HEAP.free();
        }
        if outcome.is_err() {
            break;
        }
    }
    let wall: Duration = started.elapsed() / repeats;
    let instructions = counters::stop() / repeats;

    match outcome {
        Ok(()) => {
            // The benchmark's own `elapsed:` line is the figure that is
            // comparable with the C port's; everything else it printed is the
            // answer it computed, which is what says the run was correct.
            let output = vm.output_str();
            let reported = output
                .lines()
                .find_map(|line| line.strip_prefix("elapsed: "))
                .unwrap_or("-");
            let answer: String = output
                .lines()
                .filter(|line| !line.starts_with("elapsed:"))
                .last()
                .unwrap_or("")
                .into();

            println!(
                "{name:<14} elapsed: {reported:<20} wall {} us  heap {} B{}",
                wall.as_micros(),
                free_before.saturating_sub(free_after),
                match repeats {
                    1 => alloc::string::String::new(),
                    n => alloc::format!("  (mean of {n})"),
                }
            );
            // **The placement-invariant figure.** Seconds move by three to
            // four per cent with code layout alone; this does not, so it is
            // what says whether a change removed work.
            println!("{:<14} [work] {instructions} instructions retired", "");
            println!("{:<14} -> {answer}", "");
        }
        Err(error) => {
            println!("{name:<14} ERROR line {}: {}", error.line, error.message);
        }
    }
}
