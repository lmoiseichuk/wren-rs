//! What one bytecode instruction costs, measured rather than inferred.
//!
//! The VM runs about 112 machine instructions per bytecode instruction where
//! upstream C Wren runs near seventeen. That figure is an average over a whole
//! program, which is no help in deciding what to change: it does not say
//! whether a call costs forty or four hundred.
//!
//! **This differences programs that are identical except for one construct in
//! the loop body**, and reads instructions retired from the chip's performance
//! counter. Each program is run at `N` and at `2N` iterations and the
//! difference taken, which cancels compilation, start-up and everything else
//! that happens once -- so what is left is exactly the loop, `N` times.
//!
//! Subtracting the empty loop from each of the others then gives the cost of
//! the construct that was added, in machine instructions. See
//! `doc/wren-rs/profiling.md`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use esp_backtrace as _;
use esp_hal::main;
use esp_println::println;
use wren::Vm;

#[path = "../appdesc.rs"]
mod appdesc;
use appdesc as _;

#[path = "../counters.rs"]
mod counters;

const HEAP_BYTES: usize = 320 * 1024;

/// The loop, with a hole for the body being priced.
///
/// **Everything lives in locals of one method.** A module-level variable
/// compiles to `LoadModuleVar`, which is a different instruction with a
/// different cost, so measuring a call would be measuring that too.
fn program(body: &str, rounds: u32) -> String {
    format!(
        "class Box {{\n\
         \x20 construct new() {{ _v = 1 }}\n\
         \x20 get {{ _v }}\n\
         \x20 static drive(n, b) {{\n\
         \x20   var i = 0\n\
         \x20   while (i < n) {{\n\
         \x20     {body}\n\
         \x20     i = i + 1\n\
         \x20   }}\n\
         \x20   return i\n\
         \x20 }}\n\
         }}\n\
         System.print(Box.drive({rounds}, Box.new()))\n"
    )
}

/// Instructions retired running one program, less what running it once costs.
fn cost_per_round(body: &str, rounds: u32) -> Option<u64> {
    let small = run(&program(body, rounds))?;
    let large = run(&program(body, rounds * 2))?;
    Some(large.saturating_sub(small))
}

fn run(source: &str) -> Option<u64> {
    let mut vm = Vm::new();
    counters::start(counters::INSTRUCTIONS);
    let outcome = vm.interpret(source);
    let instructions = counters::stop();
    match outcome {
        Ok(()) => Some(instructions as u64),
        Err(error) => {
            println!("  failed: {}", error.message());
            None
        }
    }
}

const ROUNDS: u32 = 20_000;

#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    esp_alloc::heap_allocator!(size: HEAP_BYTES);

    // Each body, and the bytecode it adds to one iteration of the loop.
    let bodies: [(&str, &str); 5] = [
        ("", "the empty loop itself"),
        ("i", "LoadLocal, Pop"),
        ("i + i", "LoadLocal x2, Call(+), Pop"),
        ("b.get", "LoadLocal, Call(get), LoadFieldThis, Return, Pop"),
        ("b.get + b.get", "that twice, and a Call(+)"),
    ];

    println!();
    println!("what one construct costs, in machine instructions per iteration");
    println!("({ROUNDS} iterations differenced against {}, so compilation cancels)", ROUNDS * 2);
    println!();
    println!("{:<18} {:>12} {:>12}  {}", "body", "per round", "vs empty", "adds");
    println!("{}", "-".repeat(78));

    let mut empty = 0u64;
    for (index, (body, adds)) in bodies.iter().enumerate() {
        let Some(total) = cost_per_round(body, ROUNDS) else {
            continue;
        };
        let per_round = total / u64::from(ROUNDS);
        if index == 0 {
            empty = per_round;
            println!("{:<18} {per_round:>12} {:>12}  {adds}", "(empty)", "-");
        } else {
            let label = match body.len() {
                0 => "(empty)",
                _ => body,
            };
            println!(
                "{label:<18} {per_round:>12} {:>12}  {adds}",
                per_round.saturating_sub(empty)
            );
        }
    }

    println!();
    println!("done");
    loop {}
}
