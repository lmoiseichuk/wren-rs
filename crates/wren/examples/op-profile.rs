//! What a benchmark actually asks the interpreter to do, opcode by opcode.
//!
//!     cargo run -p wren --release --features profile --example op-profile
//!     cargo run -p wren --release --features profile --example op-profile method_call
//!
//! **A histogram cannot be misattributed.** A sampling profiler on this
//! project has twice named the wrong thing -- it reports the frame the sample
//! landed in, which after an instruction-fetch stall is whatever retires next
//! rather than whatever stalled. This counts what the bytecode said, which is
//! not a matter of opinion. It is a host tool for the same reason the heap
//! profiler is: what it measures is counts, and a count does not care which
//! machine ran it.
//!
//! What it is *for* is choosing which interpreter arm to work on. An arm worth
//! optimising is one that runs often here; how much each one then costs is a
//! question for the board and the chip's performance counter. See
//! `doc/wren-rs/profiling.md`.

use wren::bytecode::Op;
use wren::Vm;

fn main() {
    let only: Option<String> = std::env::args().nth(1);

    let mut files: Vec<_> = std::fs::read_dir("benchmarks/wren")
        .expect("benchmarks/wren -- run this from the repository root")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|suffix| suffix == "wren"))
        .filter(|path| match &only {
            Some(name) => path.file_stem().is_some_and(|stem| stem == name.as_str()),
            None => true,
        })
        .collect();
    files.sort();

    if files.is_empty() {
        eprintln!("no benchmark matched {only:?}");
        std::process::exit(1);
    }

    for path in files {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&path).expect("benchmark source");

        let mut vm = Vm::new();
        if let Err(error) = vm.interpret(&source) {
            eprintln!("{name}: {}", error.message());
            continue;
        }

        let total: u64 = vm.op_counts.iter().sum();
        println!();
        println!("{name}: {total} instructions executed");
        println!();
        println!("{:<20} {:>14} {:>8}", "opcode", "count", "share");
        println!("{}", "-".repeat(56));

        // Sorted by count, because the question is always "what runs most".
        let mut rows: Vec<(u8, u64)> = vm
            .op_counts
            .iter()
            .enumerate()
            .filter(|(_, &count)| count > 0)
            .map(|(byte, &count)| (byte as u8, count))
            .collect();
        rows.sort_by_key(|&(_, count)| core::cmp::Reverse(count));

        let mut running = 0u64;
        for (byte, count) in rows {
            running += count;
            let label = match Op::from_byte(byte) {
                Some(op) => format!("{op:?}"),
                None => format!("<{byte}>"),
            };
            println!(
                "{label:<20} {count:>14} {:>7.2}%  {:>6.1}% cumulative",
                count as f64 * 100.0 / total as f64,
                running as f64 * 100.0 / total as f64,
            );
        }
    }
}
