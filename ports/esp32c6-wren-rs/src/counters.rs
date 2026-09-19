//! The chip's performance counter, for measuring work rather than time.
//!
//! **Why time is not enough here.** Two builds of the same source differ by
//! three to four per cent purely in where their instructions land -- see "What
//! a measurement on this board is worth" in `doc/wren-rs/design.md`. That is
//! larger than most changes worth making, so seconds cannot say whether a
//! change did less work. A count of instructions retired can: it does not move
//! when the code moves.
//!
//! **This is not the RISC-V standard counter.** Reading `minstret` raises an
//! illegal-instruction exception on this part; the privileged spec's machine
//! counters are not implemented. What is implemented is Espressif's own, three
//! custom CSRs that ESP-IDF declares for this chip with `SOC_CPU_HAS_CSR_PC`
//! and saves across a sleep:
//!
//! | CSR | name | holds |
//! |---|---|---|
//! | `0x7e0` | `mpcer` | which events to count, one bit each |
//! | `0x7e1` | `mpcmr` | bit 0 enables counting |
//! | `0x7e2` | `mpccr` | the count |
//!
//! ESP-IDF names one event, `PCER_CYCLES = 1 << 0`. The rest were found by
//! experiment -- see `src/bin/counters.rs`, which runs workloads differing by
//! one known operation and reports which event moves.

use core::arch::asm;

/// Count clock cycles. The one event ESP-IDF names.
pub const CYCLES: u32 = 0;
/// Count instructions retired.
pub const INSTRUCTIONS: u32 = 1;

/// Point the counter at one event and start it from zero.
pub fn start(event_bit: u32) {
    // SAFETY: these CSRs hold the performance counter and nothing else. No
    // memory, no register the compiler allocated, and no control flow depends
    // on them, so writing them cannot invalidate anything it assumed.
    unsafe {
        asm!("csrw 0x7e1, zero");           // stop
        asm!("csrw 0x7e0, {0}", in(reg) 1u32 << event_bit);
        asm!("csrw 0x7e2, zero");           // zero the count
        asm!("csrw 0x7e1, {0}", in(reg) 1u32);  // go
    }
}

/// Stop counting and read what it reached.
///
/// Stopping before the read keeps the read itself out of the count, which
/// matters when the figure is compared between builds.
pub fn stop() -> u32 {
    let value: u32;
    // SAFETY: as above.
    unsafe {
        asm!("csrw 0x7e1, zero");
        asm!("csrr {0}, 0x7e2", out(reg) value);
    }
    value
}
