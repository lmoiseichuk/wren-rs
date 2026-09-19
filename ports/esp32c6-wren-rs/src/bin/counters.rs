//! What the C6's performance counter can actually count.
//!
//! **The part has one, and it is not the RISC-V standard one.** Reading
//! `minstret` raises an illegal-instruction exception here: the machine
//! counters of the privileged spec are not implemented. What is implemented is
//! Espressif's own, three custom CSRs that ESP-IDF saves across a sleep and
//! declares for this chip with `SOC_CPU_HAS_CSR_PC`:
//!
//! | CSR | name | what it holds |
//! |---|---|---|
//! | `0x7e0` | `mpcer` | which events to count, one bit each |
//! | `0x7e1` | `mpcmr` | bit 0 enables counting |
//! | `0x7e2` | `mpccr` | the count |
//!
//! ESP-IDF names exactly one event, `PCER_CYCLES = 1 << 0`. The rest are in
//! the chip's manual and not in any header here, so this binary finds them by
//! experiment: it sets one event bit at a time and runs a workload of a known
//! shape twice, at N and 2N iterations. An event that is really counting work
//! doubles; one that is counting something fixed does not move; one that is
//! not implemented stays at zero.
//!
//! **Why it is worth the trouble.** Two builds of the same source differ by
//! three to four per cent purely in where their instructions land, which is
//! enough to swamp most changes worth making. A count of instructions retired
//! does not move when the code moves, so it separates "this does less work"
//! from "this got luckier placement" -- and a count of instruction-cache
//! misses would say directly whether the interpreter is fetch-bound, which is
//! so far an inference from how it responds to padding.

#![no_std]
#![no_main]

extern crate alloc;

use core::arch::asm;
use esp_backtrace as _;
use esp_hal::main;
use esp_println::{print, println};

#[path = "../appdesc.rs"]
mod appdesc;
use appdesc as _;

const HEAP_BYTES: usize = 32 * 1024;

/// Which events to count. One bit per event; only bit 0 is documented.
const MPCER: u32 = 0x7e0;
/// Counting is on while bit 0 is set.
const MPCMR: u32 = 0x7e1;
/// The counter itself.
const MPCCR: u32 = 0x7e2;

/// Write a custom CSR.
///
/// **The CSR number has to be a literal in the instruction**, so there is one
/// of these per register rather than one taking an address -- `csrw` encodes
/// the number in its immediate field and no register can stand in for it.
/// `const` generics would express that and `asm!` does not accept them here,
/// so a small `match` does the job.
fn write_csr(csr: u32, value: u32) {
    // SAFETY: writing a performance-counter CSR has no effect on anything the
    // compiler is tracking -- no memory, no registers it allocated, no control
    // flow. The counter is the whole architectural state involved.
    unsafe {
        match csr {
            MPCER => asm!("csrw 0x7e0, {0}", in(reg) value),
            MPCMR => asm!("csrw 0x7e1, {0}", in(reg) value),
            MPCCR => asm!("csrw 0x7e2, {0}", in(reg) value),
            _ => {}
        }
    }
}

/// Read the counter.
fn read_counter() -> u32 {
    let value: u32;
    // SAFETY: as above -- a read of a counter CSR into a register the compiler
    // gave us.
    unsafe {
        asm!("csrr {0}, 0x7e2", out(reg) value);
    }
    value
}

/// Somewhere for the load and store workloads to touch.
static mut SCRATCH: u32 = 1;

/// The baseline loop: a multiply, an add, and the loop's own arithmetic.
///
/// `read_volatile` on the accumulator is what stops the optimiser folding the
/// whole loop into a closed form, which it will otherwise do -- and then every
/// event would read zero and look unimplemented.
#[inline(never)]
fn base(rounds: u32) -> u32 {
    let mut accumulator: u32 = 1;
    for round in 0..rounds {
        accumulator = accumulator.wrapping_mul(1_664_525).wrapping_add(round);
        // SAFETY: reading a local through a pointer to itself, always valid;
        // the point is only that the optimiser cannot see through it.
        accumulator = unsafe { core::ptr::read_volatile(&accumulator) };
    }
    accumulator
}

/// The baseline plus one arithmetic instruction per iteration.
#[inline(never)]
fn plus_arithmetic(rounds: u32) -> u32 {
    let mut accumulator: u32 = 1;
    for round in 0..rounds {
        accumulator = accumulator.wrapping_mul(1_664_525).wrapping_add(round);
        accumulator = accumulator.wrapping_add(7);
        accumulator = unsafe { core::ptr::read_volatile(&accumulator) };
    }
    accumulator
}

/// The baseline plus one load from memory per iteration.
#[inline(never)]
fn plus_load(rounds: u32) -> u32 {
    let mut accumulator: u32 = 1;
    for round in 0..rounds {
        accumulator = accumulator.wrapping_mul(1_664_525).wrapping_add(round);
        // SAFETY: a volatile read of a static this binary owns, single core,
        // no concurrent writer.
        accumulator ^= unsafe { core::ptr::read_volatile(&raw const SCRATCH) };
        accumulator = unsafe { core::ptr::read_volatile(&accumulator) };
    }
    accumulator
}

/// The baseline plus one store to memory per iteration.
#[inline(never)]
fn plus_store(rounds: u32) -> u32 {
    let mut accumulator: u32 = 1;
    for round in 0..rounds {
        accumulator = accumulator.wrapping_mul(1_664_525).wrapping_add(round);
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(&raw mut SCRATCH, accumulator) };
        accumulator = unsafe { core::ptr::read_volatile(&accumulator) };
    }
    accumulator
}

/// The baseline plus one branch that is always taken.
#[inline(never)]
fn plus_branch(rounds: u32) -> u32 {
    let mut accumulator: u32 = 1;
    for round in 0..rounds {
        accumulator = accumulator.wrapping_mul(1_664_525).wrapping_add(round);
        accumulator = unsafe { core::ptr::read_volatile(&accumulator) };
        if accumulator != 0xdead_beef {
            accumulator = accumulator.wrapping_add(1);
        }
    }
    accumulator
}

/// Count one event over one workload.
fn count(event_bit: u32, work: fn(u32) -> u32, rounds: u32) -> u32 {
    write_csr(MPCMR, 0); // stop
    write_csr(MPCER, 1 << event_bit);
    write_csr(MPCCR, 0); // zero it
    write_csr(MPCMR, 1); // go
    let answer = work(rounds);
    write_csr(MPCMR, 0); // stop before reading, so the read is not counted
    core::hint::black_box(answer);
    read_counter()
}

const ROUNDS: u32 = 10_000;

#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    esp_alloc::heap_allocator!(size: HEAP_BYTES);

    let workloads: [(&str, fn(u32) -> u32); 5] = [
        ("base", base),
        ("+arith", plus_arithmetic),
        ("+load", plus_load),
        ("+store", plus_store),
        ("+branch", plus_branch),
    ];

    println!();
    println!("ESP32-C6 performance counter (CSR 0x7e0/0x7e1/0x7e2)");
    println!("counts per iteration over {ROUNDS} iterations; each workload adds");
    println!("one known operation to `base`, so the column that goes up by one");
    println!("is the event that counts that operation.");
    println!();

    print!("{:<8}", "event");
    for (name, _) in workloads.iter() {
        print!("{name:>9}");
    }
    println!();
    println!("{}", "-".repeat(8 + 9 * workloads.len()));

    for event_bit in 0..16u32 {
        let counts: [u32; 5] =
            core::array::from_fn(|index| count(event_bit, workloads[index].1, ROUNDS));
        if counts.iter().all(|&value| value == 0) {
            continue;
        }
        print!("{event_bit:<8}");
        for value in counts.iter() {
            print!("{:>9.2}", *value as f64 / ROUNDS as f64);
        }
        println!();
    }

    println!();
    println!("done");
    loop {}
}
