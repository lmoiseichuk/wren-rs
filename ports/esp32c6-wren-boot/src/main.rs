//! A node that looks for `boot` and then `main`, and runs what it finds.
//!
//! Built twice from this one file. Without the `compiler` feature it looks for
//! `boot.wrenc` and `main.wrenc` and loads them; with it, it looks for
//! `boot.wren` and `main.wren` and compiles them on the device. Everything
//! either side of that step -- the heap, the VM, the order, the accounting --
//! is shared code, so the difference between the two images is the difference
//! between the two strategies and nothing else.
//!
//! Both builds report the same three numbers per program, and the split
//! between the first two is the whole argument:
//!
//! * **prepare** -- compiling the source, or loading the bytecode.
//! * **run** -- executing it, which should be identical either way.
//! * **heap** -- what the program left resident.
//!
//! The two programs share one VM, which is what makes this a boot sequence
//! rather than two unrelated runs. They do not share *names* -- Wren rejects a
//! top-level name that is never defined, so a class declared in `boot` is not
//! visible in `main` -- and `doc/examples/boot.wren` says more about why.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use esp_backtrace as _;
use esp_hal::main;
use esp_hal::time::Instant;
use esp_println::println;
use wren::Vm;

mod appdesc;
mod programs;
use programs::{Program, EXTENSION, FLAVOUR, SEQUENCE};

/// The heap the VM gets, matching every other wren-rs measurement here so the
/// numbers can be set beside them.
const HEAP_BYTES: usize = 320 * 1024;

/// What `prepare` hands to `run`.
///
/// A type alias rather than an enum: each build has exactly one of these, and
/// an enum would carry a discriminant and a dead variant into an image whose
/// size is the thing being measured.
#[cfg(feature = "compiler")]
type Prepared = alloc::rc::Rc<wren::bytecode::Chunk>;
#[cfg(not(feature = "compiler"))]
type Prepared = wren::ObjectId;

#[main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    esp_alloc::heap_allocator!(size: HEAP_BYTES);

    // As near to reset as this code can observe. It misses the second-stage
    // bootloader, which is the same for both builds, so the difference between
    // two of these is still meaningful even though neither is absolute.
    let powered = Instant::now();

    println!();
    println!("wren-rs node, {FLAVOUR}");
    println!("heap         : {HEAP_BYTES} B");
    print_store();

    // Measured on a VM that is built and dropped, so it is the cost of having
    // a VM at all rather than of anything the programs went on to do.
    let before_vm = esp_alloc::HEAP.free();
    let probe = Vm::new();
    let resident = before_vm.saturating_sub(esp_alloc::HEAP.free());
    drop(probe);
    println!("vm resident  : {resident} B");
    println!();

    let mut vm = Vm::new();
    let origin = Instant::now();
    vm.set_clock(move || origin.elapsed().as_micros() as f64 / 1_000_000.0);

    // The VM accumulates everything its programs print, so each stage reports
    // only what appeared since the last one.
    let mut printed = 0usize;
    let mut prepare_total = 0u64;
    let mut run_total = 0u64;

    for stem in SEQUENCE {
        match programs::find(stem) {
            // A missing program is the ordinary case, not a fault: a node with
            // no `boot` simply has nothing to set up.
            None => println!("[{stem}] not present, skipped"),
            Some(program) => {
                let (prepare, run) = stage(&mut vm, program, &mut printed);
                prepare_total += prepare;
                run_total += run;
            }
        }
    }

    println!();
    println!(
        "total        : prepare {prepare_total} us  run {run_total} us  \
         reset-to-idle {} us",
        powered.elapsed().as_micros()
    );
    println!("heap left    : {} B", esp_alloc::HEAP.free());
    println!("done");

    // Nothing left to run. A node with a job would sleep here; this one only
    // has to stop without pretending the core is busy.
    loop {
        core::hint::spin_loop();
    }
}

/// Say what the node found before it runs any of it.
fn print_store() {
    let mut total = 0usize;
    for program in programs::STORE {
        total += program.bytes.len();
    }
    println!(
        "programs     : {} in store, {total} B of .{EXTENSION}",
        programs::STORE.len()
    );
    for program in programs::STORE {
        println!(
            "               {:<12} {} B",
            program.name,
            program.bytes.len()
        );
    }
}

/// Prepare and run one program, reporting what it cost and what it said.
///
/// Returns the prepare and run times in microseconds so the caller can total
/// them; the per-program line is printed here because a failure has to be
/// reported where the error is still in hand.
fn stage(vm: &mut Vm, program: &Program, printed: &mut usize) -> (u64, u64) {
    let free_before = esp_alloc::HEAP.free();

    let started = Instant::now();
    let prepared = match prepare(vm, program.bytes) {
        Ok(prepared) => prepared,
        Err(reason) => {
            println!("[{}] PREPARE FAILED: {reason}", program.name);
            return (started.elapsed().as_micros(), 0);
        }
    };
    let prepare_time = started.elapsed().as_micros();

    let started = Instant::now();
    let outcome = run(vm, prepared);
    let run_time = started.elapsed().as_micros();

    let heap = free_before.saturating_sub(esp_alloc::HEAP.free());
    println!(
        "[{}] prepare {prepare_time} us  run {run_time} us  heap {heap} B",
        program.name
    );

    // The program's own output, indented under the line that accounts for it.
    let output = vm.output_str();
    if output.len() > *printed {
        for line in output[*printed..].lines() {
            println!("    {line}");
        }
        *printed = output.len();
    }

    if let Err(reason) = outcome {
        println!("    ERROR: {reason}");
    }
    (prepare_time, run_time)
}

/// Compile the source into a chunk.
#[cfg(feature = "compiler")]
fn prepare(vm: &mut Vm, bytes: &'static [u8]) -> Result<Prepared, String> {
    let source = match core::str::from_utf8(bytes) {
        Ok(source) => source,
        Err(_) => return Err(String::from("the source is not utf-8")),
    };
    match wren::compiler::compile(vm, source) {
        Ok(chunk) => Ok(alloc::rc::Rc::new(chunk)),
        Err(error) => Err(format!("line {}: {}", error.line, error.message)),
    }
}

/// Read the bytecode into this VM.
///
/// The counterpart of the compile above, and the reason this port exists: the
/// names are re-interned and the operands rewritten, which is work, but it is
/// not parsing.
#[cfg(not(feature = "compiler"))]
fn prepare(vm: &mut Vm, bytes: &'static [u8]) -> Result<Prepared, String> {
    match wren::wrenc::load(vm, bytes) {
        Ok(loaded) => Ok(loaded.closure),
        Err(error) => Err(error.message()),
    }
}

#[cfg(feature = "compiler")]
fn run(vm: &mut Vm, prepared: Prepared) -> Result<(), String> {
    match vm.run(prepared) {
        Ok(()) => Ok(()),
        Err(error) => Err(format!("line {}: {}", error.line, error.message)),
    }
}

#[cfg(not(feature = "compiler"))]
fn run(vm: &mut Vm, prepared: Prepared) -> Result<(), String> {
    match vm.run_closure(prepared) {
        Ok(()) => Ok(()),
        Err(error) => Err(format!("line {}: {}", error.line, error.message)),
    }
}
