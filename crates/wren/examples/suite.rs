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

    // **Run the files across threads.** Each program gets a fresh `Vm` and
    // shares nothing with any other, so this is embarrassingly parallel and was
    // only serial because it started that way. On this bench it is the
    // difference between 56 seconds and about four, which matters because this
    // runs after every change.
    //
    // Results are collected with their paths and sorted afterwards, so the
    // report is identical whatever order the threads happen to finish in -- a
    // run whose output shuffles between invocations is useless for spotting
    // what a change actually did.
    let outcomes = run_in_parallel(&files, &root);

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

    for outcome in &outcomes {
        total += 1;
        let entry = by_group.entry(outcome.group.clone()).or_insert((0, 0));
        entry.1 += 1;
        if outcome.is_error_test {
            error_total += 1;
        }

        match &outcome.result {
            Ok(()) => {
                passed += 1;
                entry.0 += 1;
                if outcome.is_error_test {
                    error_passed += 1;
                }
            }
            Err(reason) => {
                *reasons.entry(summarise(reason)).or_insert(0) += 1;
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

    // `--why <substring>` lists the files behind one reason, which is how the
    // histogram turns into something to act on.
    if std::env::args().nth(2).as_deref() == Some("--slow") {
        let mut ranked: Vec<&Outcome> = outcomes.iter().collect();
        ranked.sort_by_key(|outcome| core::cmp::Reverse(outcome.elapsed));
        println!("\nslowest files:");
        for outcome in ranked.iter().take(15) {
            println!("  {:>8.2}s  {}", outcome.elapsed.as_secs_f64(), outcome.path.display());
        }
        return;
    }

    // `--diff <path substring>` shows expected against actual, line by line,
    // for the files that match. The histogram says how many; this says what.
    if std::env::args().nth(2).as_deref() == Some("--diff") {
        let wanted = std::env::args().nth(3).unwrap_or_default();
        for outcome in &outcomes {
            let path = outcome.path.display().to_string();
            if !path.contains(&wanted) || outcome.result.is_ok() {
                continue;
            }
            println!("\n--- {path}");
            if let Err(reason) = &outcome.result {
                if !reason.starts_with("output differs") {
                    println!("    {reason}");
                    continue;
                }
            }
            let Ok(source) = std::fs::read_to_string(&outcome.path) else { continue };
            let expected: Vec<String> = source.lines().filter_map(expectation).collect();
            let directory = outcome.path.parent().map(Path::to_path_buf).unwrap_or_default();
            let mut vm = wren::Vm::new();
            let name = {
                let text = outcome.path.display().to_string();
                let text = text.strip_prefix("vendor/wren/").unwrap_or(&text);
                let text = text.strip_suffix(".wren").unwrap_or(text);
                format!("./{text}")
            };
            vm.set_main_module_name(&name);
            let base = directory.clone();
            vm.set_module_loader(move |name| {
                std::fs::read_to_string(base.join(format!("{}.wren", name.trim_start_matches("./")))).ok()
            });
            let _ = vm.interpret(&source);
            let got: Vec<&str> = vm.output_str().lines().collect();
            for index in 0..expected.len().max(got.len()) {
                let want = expected.get(index).map(String::as_str).unwrap_or("<none>");
                let have = got.get(index).copied().unwrap_or("<none>");
                if want != have {
                    println!("    {index}: got {have:?} want {want:?}");
                }
            }
        }
        return;
    }

    if let Some(wanted) = std::env::args().nth(2) {
        println!("\nfiles failing with {wanted:?}:");
        for outcome in &outcomes {
            if let Err(reason) = &outcome.result {
                if reason.contains(&wanted) {
                    println!("  {}", outcome.path.display());
                }
            }
        }
        return;
    }

    println!("\nwhy the rest fail, most common first:");
    let mut ranked: Vec<_> = reasons.into_iter().collect();
    ranked.sort_by_key(|(_, count)| core::cmp::Reverse(*count));
    for (reason, count) in ranked.iter().take(15) {
        println!("  {count:>4}  {reason}");
    }
}

/// What one file did.
struct Outcome {
    path: PathBuf,
    group: String,
    is_error_test: bool,
    result: Result<(), String>,
    /// How long the program took. Worth recording because a slow test is
    /// usually not a big test -- it is one running longer than it should,
    /// which points at a loop that is not terminating the way it ought to.
    elapsed: std::time::Duration,
}

/// Run every file, spread across the available cores.
fn run_in_parallel(files: &[PathBuf], root: &str) -> Vec<Outcome> {
    // A test program is *expected* to panic the VM sometimes -- that is one of
    // the things being measured -- so the default hook's backtrace spam would
    // bury the report. The panic is still caught and reported as a failure
    // naming the file; only the message is suppressed.
    std::panic::set_hook(Box::new(|_| {}));

    let workers = std::thread::available_parallelism().map_or(1, |count| count.get());
    let chunk = files.len().div_ceil(workers).max(1);

    let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|slice| scope.spawn(move || slice.iter().filter_map(|path| one(path, root)).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|handle| handle.join().unwrap_or_default()).collect()
    });

    let _ = std::panic::take_hook();
    let mut outcomes = outcomes;
    // Sorted so the report does not depend on which thread finished first.
    outcomes.sort_by(|left, right| left.path.cmp(&right.path));
    outcomes
}

fn one(path: &Path, root: &str) -> Option<Outcome> {
    // The benchmark and api directories are not correctness tests: the first
    // are timings and the second need the C embedding harness.
    let display = path.display().to_string();
    if display.contains("/benchmark/") || display.contains("/api/") {
        return None;
    }
    let source = std::fs::read_to_string(path).ok()?;

    // **`// nontest` means this file is a fixture, not a test.** Upstream marks
    // the modules its import tests load, and counting them as tests in their
    // own right scores a helper against expectations it never had -- 25 files
    // that were quietly making the denominator and the failure list both wrong.
    if source.lines().next().is_some_and(|line| line.contains("nontest")) {
        return None;
    }

    let is_error_test = is_error_test(&source);

    // **A panic in the VM is a failure, not the end of the run.** A bad
    // program must not take the harness down with it, and the file that did it
    // is the thing worth knowing.
    let started = std::time::Instant::now();
    let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
    // **Upstream names the main module after the file it was given**, turning
    // `test/meta/x.wren` into `./test/meta/x` (test/test.c). A program can ask
    // `Meta.getModuleVariables` about itself, so it needs to be findable under
    // that name.
    let module_name = {
        let text = path.display().to_string();
        let text = text.strip_prefix("vendor/wren/").unwrap_or(&text);
        let text = text.strip_suffix(".wren").unwrap_or(text);
        format!("./{text}")
    };
    let result = std::panic::catch_unwind(|| check(&source, &directory, &module_name))
        .unwrap_or_else(|_| Err(format!("PANIC in {}", path.display())));
    let elapsed = started.elapsed();

    Some(Outcome {
        path: path.to_path_buf(),
        group: group_of(path, root),
        is_error_test,
        result,
        elapsed,
    })
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
fn check(source: &str, directory: &Path, module_name: &str) -> Result<(), String> {
    let mut expected = Vec::new();

    for line in source.lines() {
        if let Some(text) = expectation(line) {
            expected.push(text);
        }
    }

    // **The same three markers `one` classifies with.** They disagreed before:
    // this function tested two of them, so a file marked only `// expect error`
    // was scored as an output test expecting no lines, and every one of them
    // was counted a failure for raising the error it asked for.
    let expects_error = is_error_test(source);

    let mut vm = wren::Vm::new();

    // **The VM resolves relative imports itself**, against the importing
    // module's name, so what reaches the loader is already a full path from
    // the test root -- `./test/language/module/x/module`. The loader's job is
    // only to turn that into a file. It was joining against the test's own
    // directory, which double-counted the path once the main module had a
    // name to resolve against.
    let _ = directory;
    vm.set_module_loader(move |name| {
        let relative = name.trim_start_matches("./");
        std::fs::read_to_string(format!("vendor/wren/{relative}.wren")).ok()
    });

    vm.set_main_module_name(module_name);

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

/// The text a `// expect:` comment asks for, if the line carries one.
///
/// **The space after the colon is optional**, as it is in upstream's own
/// runner (`util/test.py`, `// expect: ?(.*)`), so a bare `// expect:` asks
/// for an *empty* line. Requiring the space dropped that expectation and
/// scored a correct blank line as a surplus one. Both places that read
/// expectations go through here, because they disagreed once already.
fn expectation(line: &str) -> Option<String> {
    let at = line.find("// expect:")?;
    let rest = &line[at + "// expect:".len()..];
    Some(rest.strip_prefix(' ').unwrap_or(rest).to_string())
}

/// Does this file expect to fail?
fn is_error_test(source: &str) -> bool {
    source.contains("// expect runtime error:")
        || source.contains("Error at")
        || source.contains("// expect error")
}

/// Collapse a message to something worth counting.
fn summarise(reason: &str) -> String {
    if reason.starts_with("PANIC") {
        return reason.to_string();
    }
    if let Some(at) = reason.find(" does not implement ") {
        return format!("missing method{}", &reason[at + 19..]);
    }
    if reason.starts_with("output differs") {
        return "output differs".to_string();
    }
    reason.to_string()
}
