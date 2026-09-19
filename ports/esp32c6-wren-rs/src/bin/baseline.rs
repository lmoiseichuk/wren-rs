//! The same firmware with no VM in it, to measure what the VM costs.
//!
//! **An image size on its own says very little.** This port is bare metal and
//! the C port is an ESP-IDF application, so their totals are not comparable:
//! one carries FreeRTOS and newlib, the other carries none of it. What *is*
//! comparable is how much each adds to its own platform's empty application,
//! and that is what this binary establishes for this side.
//!
//! It links the same runtime, the same allocator and the same printing, and
//! differs only in never mentioning `wren`.

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::main;
use esp_hal::time::Instant;
use esp_println::println;

#[path = "../appdesc.rs"]
mod appdesc;
use appdesc as _;

const HEAP_BYTES: usize = 320 * 1024;

#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    esp_alloc::heap_allocator!(size: HEAP_BYTES);

    // Enough of the same work that the allocator, the float formatting and the
    // timer are all linked -- the VM uses all three, so leaving them out of
    // the baseline would charge them to the VM.
    let started = Instant::now();
    let mut total = alloc::vec::Vec::new();
    for index in 0..8 {
        total.push(index as f64 * 1.5);
    }
    let sum: f64 = total.iter().sum();
    println!("baseline {sum} in {} us, heap {HEAP_BYTES} B", started.elapsed().as_micros());

    loop {}
}
