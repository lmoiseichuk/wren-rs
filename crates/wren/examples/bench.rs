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
//!
//! **`--repeat N` is for optimisation work, not for publishing.** A single run
//! of these on a workstation is a few tens of milliseconds, which is inside
//! the noise of anything a machine does between two processes -- so comparing
//! one run against another cannot tell a 20% win from a busy core. Repeating
//! and taking the *fastest* run does: the minimum is the one least disturbed
//! by whatever else the machine was doing, where a mean is dragged around by
//! exactly that.

use std::time::Instant;

fn main() {
    let repeat: usize = std::env::args()
        .skip_while(|argument| argument != "--repeat")
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    // A bare argument names one benchmark, which is what profiling wants: the
    // four have completely different shapes and a mixed profile tells you
    // about the average of them, which is nothing.
    let only: Option<String> = std::env::args()
        .skip(1)
        .find(|argument| !argument.starts_with("--") && argument.parse::<usize>().is_err());

    let mut files: Vec<_> = std::fs::read_dir("benchmarks/wren")
        .expect("benchmarks/wren")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "wren")
        })
        .filter(|path| match &only {
            Some(name) => path.file_stem().is_some_and(|stem| stem == name.as_str()),
            None => true,
        })
        .collect();
    files.sort();

    if repeat > 1 {
        println!("best of {repeat}");
    }
    println!(
        "{:<16} {:>10} {:>12}  output",
        "benchmark", "wall (s)", "self (s)"
    );
    println!("{}", "-".repeat(64));

    for path in files {
        let source = std::fs::read_to_string(&path).expect("read");
        let name = path.file_stem().unwrap().to_string_lossy().to_string();

        // A fresh VM per run: reusing one would leave the previous run's heap
        // in place and measure the collector against a warm heap in the later
        // runs and a cold one in the first.
        let mut wall = f64::MAX;
        let mut vm = wren::Vm::new();
        let mut result = Ok(());
        for _ in 0..repeat {
            vm = wren::Vm::new();
            let started = Instant::now();
            result = vm.interpret(&source);
            wall = wall.min(started.elapsed().as_secs_f64());
            if result.is_err() {
                break;
            }
        }

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
                println!(
                    "{name:<16} {:>10} {:>12}  ERROR line {}: {}",
                    "-",
                    "-",
                    error.line(),
                    error.message()
                );
            }
        }
    }
}
