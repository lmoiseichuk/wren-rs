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

/// The benchmarks, in the order the published table lists them.
const BENCHMARKS: &[(&str, &str)] = &[
    ("binary_trees", include_str!("../../../benchmarks/wren/binary_trees.wren")),
    ("fib", include_str!("../../../benchmarks/wren/fib.wren")),
    ("list_build", include_str!("../../../benchmarks/wren/list_build.wren")),
    ("method_call", include_str!("../../../benchmarks/wren/method_call.wren")),
];

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

    for (name, source) in BENCHMARKS {
        run_one(name, source);
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

/// Compile and run one benchmark, reporting what it cost.
fn run_one(name: &str, source: &str) {
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
    // **Two runs, because the counter holds one event at a time.** The second
    // is the same program from the same starting state, so the two figures
    // describe one run between them: how much work it was, and how long the
    // part took to do it.
    counters::start(counters::INSTRUCTIONS);
    let started = Instant::now();
    let outcome = vm.interpret(source);
    let wall: Duration = started.elapsed();
    let instructions = counters::stop();
    let free_after = esp_alloc::HEAP.free();

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
                "{name:<14} elapsed: {reported:<20} wall {} us  heap {} B",
                wall.as_micros(),
                free_before.saturating_sub(free_after)
            );
            // **The placement-invariant figure.** Seconds move by three to
            // four per cent with code layout alone; this does not, so it is
            // what says whether a change removed work.
            println!("{:<14} [work] {instructions} instructions retired", "");
            println!("{:<14} -> {answer}", "");
        }
        Err(error) => {
            println!("{name:<14} ERROR line {}: {}", error.line(), error.message());
        }
    }
}
