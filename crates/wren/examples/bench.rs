//! Run the benchmark programs on this implementation.
//!
//!     cargo run -p wren --release --example bench
//!
//! **The same files the C port and MicroPython were measured on** --
//! `benchmarks/wren/*.wren`, unchanged -- so the numbers can be set beside
//! `doc/wren/benchmarks.md` rather than only beside each other.
//!
//! A host run is not the comparison that counts: the published figures are
//! from an ESP32-C6, and a workstation says nothing about a part with no FPU
//! and a 160 MHz clock. What it does say is whether the programs run at all
//! and produce the right answers, which has to be true before any timing means
//! anything.

use std::time::Instant;

fn main() {
    let mut files: Vec<_> = std::fs::read_dir("benchmarks/wren")
        .expect("benchmarks/wren")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "wren"))
        .collect();
    files.sort();

    println!("{:<16} {:>10} {:>12}  output", "benchmark", "wall (s)", "self (s)");
    println!("{}", "-".repeat(64));

    for path in files {
        let source = std::fs::read_to_string(&path).expect("read");
        let name = path.file_stem().unwrap().to_string_lossy().to_string();

        let mut vm = wren::Vm::new();
        let started = Instant::now();
        let result = vm.interpret(&source);
        let wall = started.elapsed().as_secs_f64();

        match result {
            Ok(()) => {
                let output = vm.output_str();
                // Each benchmark prints its own `elapsed:` line, which is the
                // figure the C port and MicroPython were compared on.
                let reported = output
                    .lines()
                    .find_map(|line| line.strip_prefix("elapsed: "))
                    .unwrap_or("-")
                    .to_string();
                let first = output.lines().next().unwrap_or("").to_string();
                println!("{name:<16} {wall:>10.3} {reported:>12}  {first}");
            }
            Err(error) => {
                println!("{name:<16} {:>10} {:>12}  ERROR line {}: {}", "-", "-", error.line(), error.message());
            }
        }
    }
}
