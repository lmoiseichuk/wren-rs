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
        "var module_var = 0\n\
         class Box {{\n\
         \x20 construct new() {{ _v = 1 }}\n\
         \x20 get {{ _v }}\n\
         \x20 bump {{ _v = _v + 1 }}\n\
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

/// What one round of a body costs, in retired instructions and in cycles.
///
/// **Both, because they answer different questions.** Instructions say how
/// much work an opcode is; cycles say how long the part takes to do it, and
/// the ratio between them is where a stall hides. An opcode with a poor ratio
/// is waiting on something -- a load from flash, a dependent chain -- and is
/// worth a different fix from one that simply does too much.
fn cost_per_round(body: &str, rounds: u32) -> Option<(u64, u64)> {
    let small = run(&program(body, rounds), counters::INSTRUCTIONS)?;
    let large = run(&program(body, rounds * 2), counters::INSTRUCTIONS)?;
    let small_cycles = run(&program(body, rounds), counters::CYCLES)?;
    let large_cycles = run(&program(body, rounds * 2), counters::CYCLES)?;
    Some((
        large.saturating_sub(small),
        large_cycles.saturating_sub(small_cycles),
    ))
}

fn run(source: &str, event: u32) -> Option<u64> {
    let mut vm = Vm::new();
    counters::start(event);
    let outcome = vm.interpret(source);
    let counted = counters::stop();
    match outcome {
        Ok(()) => Some(counted as u64),
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
    //
    // **Ordered so that consecutive pairs differ by about one thing**, which
    // is what makes a single opcode's cost readable off the table rather than
    // inferred from it: `1` against `i` is Constant against LoadLocal, and
    // `b.bump` against `b.get` is a field store plus an arithmetic call.
    let bodies: [(&str, &str); 10] = [
        ("", "the empty loop itself"),
        ("i", "LoadLocal, Pop"),
        ("1", "Constant, Pop"),
        ("module_var", "LoadModuleVar, Pop"),
        ("module_var = i", "LoadLocal, StoreModuleVar, Pop"),
        ("i + i", "LoadLocalPair, Call(Num.+), Pop"),
        ("i < i", "LoadLocalPair, Call(Num.<), Pop"),
        ("b.get", "LoadLocal, Call(get), LoadFieldThisReturn, Pop"),
        ("b.get + b.get", "that twice, and a Call(Num.+)"),
        ("b.bump", "Call(bump), LoadFieldThis, Constant, Call(+), StoreFieldThis"),
    ];

    println!();
    println!("what one construct costs per iteration, measured not inferred");
    println!("({ROUNDS} iterations differenced against {}, so compilation cancels)", ROUNDS * 2);
    println!();
    println!(
        "{:<18} {:>9} {:>9} {:>9} {:>9} {:>6}  {}",
        "body", "instr", "vs empty", "cycles", "vs empty", "CPI", "adds"
    );
    println!("{}", "-".repeat(104));

    let (mut empty, mut empty_cycles) = (0u64, 0u64);
    for (index, (body, adds)) in bodies.iter().enumerate() {
        let Some((total, total_cycles)) = cost_per_round(body, ROUNDS) else {
            continue;
        };
        let per_round = total / u64::from(ROUNDS);
        let cycles_per_round = total_cycles / u64::from(ROUNDS);
        let cpi = cycles_per_round as f64 / per_round.max(1) as f64;
        if index == 0 {
            empty = per_round;
            empty_cycles = cycles_per_round;
            println!(
                "{:<18} {per_round:>9} {:>9} {cycles_per_round:>9} {:>9} {cpi:>6.2}  {adds}",
                "(empty)", "-", "-"
            );
        } else {
            println!(
                "{body:<18} {per_round:>9} {:>9} {cycles_per_round:>9} {:>9} {cpi:>6.2}  {adds}",
                per_round.saturating_sub(empty),
                cycles_per_round.saturating_sub(empty_cycles)
            );
        }
    }

    println!();
    println!("done");
    loop {}
}
