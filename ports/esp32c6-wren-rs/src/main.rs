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
use wren::Vm;

// The image header the chip's bootloader looks for.
mod appdesc;
use appdesc as _;

/// The heap the VM allocates from.
///
/// **256 KB**, chosen to leave a program roughly what the C port left one.
/// That port had about 310 KB free at boot and its VM took 83,036 B of it,
/// leaving ~227 KB; matching the space available to the *program* is what
/// makes a heap figure comparable, rather than matching the total.
const HEAP_BYTES: usize = 320 * 1024;

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
    println!();

    // What one VM costs before it has run anything, which is the figure to set
    // beside the C port's 83,036 B resident.
    let before_vm = esp_alloc::HEAP.free();
    let probe = Vm::new();
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

/// Compile and run one benchmark, reporting what it cost.
fn run_one(name: &str, source: &str) {
    // A fresh VM per benchmark, so one program's garbage is never another's
    // starting condition -- the C port restarts the board between runs for the
    // same reason.
    let mut vm = Vm::new();

    // `System.clock` is a host hook on a bare-metal target: there is no
    // process clock to default to, so the port supplies the one the chip has.
    let origin = Instant::now();
    vm.set_clock(move || origin.elapsed().as_micros() as f64 / 1_000_000.0);

    let free_before = esp_alloc::HEAP.free();
    let started = Instant::now();
    let outcome = vm.interpret(source);
    let wall: Duration = started.elapsed();
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
            println!("{:<14} -> {answer}", "");
        }
        Err(error) => {
            println!("{name:<14} ERROR line {}: {}", error.line(), error.message());
        }
    }
}
