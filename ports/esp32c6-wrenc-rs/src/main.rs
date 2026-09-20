//! The same four benchmarks, run from `.wrenc` with **no compiler in the
//! image**.
//!
//! This is what step 4 of the plan is for. The device is handed bytecode built
//! on a workstation, so the lexer and the parser are not linked at all — and
//! the question this binary answers is how much of the firmware they were.
//!
//! Everything else is identical to `main.rs`: same heap, same clock, same
//! programs, same reporting. The only differences are that `wren` is built
//! without its `compiler` feature and the programs arrive already compiled.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use esp_backtrace as _;
use esp_hal::main;
use esp_hal::time::{Duration, Instant};
use esp_println::println;
use wren::Vm;

mod appdesc;
use appdesc as _;

const HEAP_BYTES: usize = 320 * 1024;

/// The compiled programs, as bytes.
const BENCHMARKS: &[(&str, &[u8])] = &[
    ("binary_trees", include_bytes!("../../../benchmarks/wrenc/binary_trees.wrenc")),
    ("fib", include_bytes!("../../../benchmarks/wrenc/fib.wrenc")),
    ("list_build", include_bytes!("../../../benchmarks/wrenc/list_build.wrenc")),
    ("method_call", include_bytes!("../../../benchmarks/wrenc/method_call.wrenc")),
];

#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    esp_alloc::heap_allocator!(size: HEAP_BYTES);

    println!();
    println!("wren-rs on ESP32-C6, from bytecode, no compiler linked");
    println!("heap         : {HEAP_BYTES} B");
    println!();

    let before_vm = esp_alloc::HEAP.free();
    let probe = Vm::new();
    let resident = before_vm.saturating_sub(esp_alloc::HEAP.free());
    drop(probe);
    println!("[vm] resident {resident} B");
    println!();

    for (name, bytes) in BENCHMARKS {
        run_one(name, bytes);
    }

    println!();
    println!("done");
    loop {}
}

fn run_one(name: &str, bytes: &[u8]) {
    // **The VM is built for this program.** Its `.wrenc` names every method
    // it can call, so the core library is installed to that list rather than
    // in full -- see `Vm::with_core_methods`. What that saves is RAM, and on
    // a part with kilobytes it is most of what the VM holds.
    let before_vm = esp_alloc::HEAP.free();
    let manifest = match wren::wrenc::manifest(bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            println!("{name:<14} MANIFEST ERROR: {}", error.message());
            return;
        }
    };
    let asked = manifest.signatures.len();
    let mut vm = Vm::with_core_methods(&manifest.signatures);
    let resident = before_vm.saturating_sub(esp_alloc::HEAP.free());
    println!("{name:<14} tailored to {asked} methods, VM resident {resident} B");
    let origin = Instant::now();
    vm.set_clock(move || origin.elapsed().as_micros() as f64 / 1_000_000.0);

    let free_before = esp_alloc::HEAP.free();

    // Loading is the whole of what a compile used to be, so it is timed
    // separately: the point of shipping bytecode is that this is small.
    let load_started = Instant::now();
    let loaded = match wren::wrenc::load(&mut vm, bytes) {
        Ok(loaded) => loaded,
        Err(error) => {
            println!("{name:<14} LOAD ERROR: {}", error.message());
            return;
        }
    };
    let load_time: Duration = load_started.elapsed();

    let started = Instant::now();
    let outcome = vm.run_closure(loaded.closure);
    let wall: Duration = started.elapsed();
    let free_after = esp_alloc::HEAP.free();

    match outcome {
        Ok(()) => {
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
                "{name:<14} elapsed: {reported:<20} wall {} us  load {} us  heap {} B",
                wall.as_micros(),
                load_time.as_micros(),
                free_before.saturating_sub(free_after)
            );
            println!("{:<14} -> {answer}", "");
        }
        Err(error) => println!("{name:<14} ERROR line {}: {}", error.line, error.message),
    }
}
