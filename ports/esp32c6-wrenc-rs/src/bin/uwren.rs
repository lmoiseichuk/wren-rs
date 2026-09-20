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

    let mut vm = Vm::with_core_methods(&manifest.signatures);
    report("after the VM");

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
