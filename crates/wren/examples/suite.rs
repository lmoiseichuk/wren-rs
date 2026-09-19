//! Run upstream's own test files against this implementation.
//!
//!     cargo run -p wren --example suite -- vendor/wren/test
//!
//! **Upstream's files, unmodified, scored against the `// expect:` comments
//! they already carry** — the same contract the C port was held to in step 1,
//! so the two pass rates mean the same thing and can be set side by side.
//!
//! Most of the suite exercises classes, functions and fibers, which this does
//! not have yet. The point is not the pass rate on its own; it is the histogram
//! of *why* things fail, which says what to build next in the order the tests
//! actually care about.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "vendor/wren/test".to_string());

    let mut files = Vec::new();
    collect(Path::new(&root), &mut files);
    files.sort();

    let mut by_group: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut passed = 0;
    let mut total = 0;
    // **Error tests are counted apart, and they should be.** One passes if the
    // program failed at all, so "unsupported syntax" scores the same as
    // "correctly rejected". Mixing them into one number would let this look
    // better the *less* of the language it implements.
    let mut error_passed = 0;
    let mut error_total = 0;

    for path in &files {
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        // The benchmark and api directories are not correctness tests: the
        // first are timings and the second need the C embedding harness.
        let display = path.display().to_string();
        if display.contains("/benchmark/") || display.contains("/api/") {
            continue;
        }

        total += 1;
        let group = group_of(path, &root);
        let entry = by_group.entry(group).or_insert((0, 0));
        entry.1 += 1;

        let is_error_test = source.contains("// expect runtime error:")
            || source.contains("Error at")
            || source.contains("// expect error");
        if is_error_test {
            error_total += 1;
        }

        match check(&source) {
            Ok(()) => {
                passed += 1;
                entry.0 += 1;
                if is_error_test {
                    error_passed += 1;
                }
            }
            Err(reason) => {
                *reasons.entry(summarise(&reason)).or_insert(0) += 1;
            }
        }
    }

    let real_passed = passed - error_passed;
    let real_total = total - error_total;
    println!("\n{passed} of {total} upstream tests pass");
    println!(
        "  {real_passed} of {real_total} produce the right output -- the number that means something"
    );
    println!(
        "  {error_passed} of {error_total} are error tests, which pass on any error at all\n"
    );

    println!("{:<14} {:>8}", "group", "passed");
    println!("{}", "-".repeat(46));
    for (group, (ok, count)) in &by_group {
        let share = if *count == 0 { 0 } else { ok * 24 / count };
        let bar: String = "#".repeat(share) + &".".repeat(24 - share);
        println!("{group:<14} {ok:>4}/{count:<4} {bar}");
    }

    println!("\nwhy the rest fail, most common first:");
    let mut ranked: Vec<_> = reasons.into_iter().collect();
    ranked.sort_by_key(|(_, count)| core::cmp::Reverse(*count));
    for (reason, count) in ranked.iter().take(15) {
        println!("  {count:>4}  {reason}");
    }
}

fn collect(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "wren") {
            out.push(path);
        }
    }
}

fn group_of(path: &Path, root: &str) -> String {
    path.strip_prefix(root)
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|first| first.as_os_str().to_string_lossy().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Run one file and decide whether it did what its comments say it should.
fn check(source: &str) -> Result<(), String> {
    let mut expected = Vec::new();
    let mut expects_error = false;

    for line in source.lines() {
        if let Some(at) = line.find("// expect: ") {
            expected.push(line[at + 11..].to_string());
        }
        if line.contains("// expect runtime error:") || line.contains("Error at") {
            expects_error = true;
        }
    }

    let mut vm = wren::Vm::new();
    let result = vm.interpret(source);

    if expects_error {
        // Only that it failed, not how. Matching upstream's exact message and
        // line for every error is a later refinement.
        return match result {
            Err(_) => Ok(()),
            Ok(()) => Err("expected an error, none raised".to_string()),
        };
    }

    match result {
        Err(error) => Err(error.message().to_string()),
        Ok(()) => {
            let printed: Vec<String> =
                vm.output_str().lines().map(|line| line.to_string()).collect();
            if printed == expected {
                Ok(())
            } else {
                Err(format!(
                    "output differs: expected {} line(s), got {}",
                    expected.len(),
                    printed.len()
                ))
            }
        }
    }
}

/// Collapse a message to something worth counting.
fn summarise(reason: &str) -> String {
    if let Some(at) = reason.find(" does not implement ") {
        return format!("missing method{}", &reason[at + 19..]);
    }
    if reason.starts_with("output differs") {
        return "output differs".to_string();
    }
    reason.to_string()
}
