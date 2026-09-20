//! What a VM costs when it carries only the methods one program calls.
//!
//!     cargo run -p wren --release --example tailored -- benchmarks/wrenc/fib.wrenc
//!
//! Builds the same program twice — once on a VM with the whole core library,
//! once on a VM built from the program's own manifest — and reports what the
//! second one saved, and that both printed the same thing.

fn main() {
    let path = std::env::args().nth(1).expect("usage: tailored <file.wrenc>");
    let bytes = std::fs::read(&path).expect("readable");
    let manifest = wren::wrenc::manifest(&bytes).expect("a .wrenc");

    let mut full = wren::Vm::new();
    let full_symbols = count_symbols(&full);
    let full_resident = full.heap.bytes();
    let loaded = wren::wrenc::load(&mut full, &bytes).expect("loads");
    full.run_closure(loaded.closure).expect("runs");
    let full_output = full.output_str().to_string();

    let mut tailored = wren::Vm::with_core_methods(&manifest.signatures);
    let tailored_symbols = count_symbols(&tailored);
    let tailored_resident = tailored.heap.bytes();
    let loaded = wren::wrenc::load(&mut tailored, &bytes).expect("loads");
    tailored.run_closure(loaded.closure).expect("runs");
    let tailored_output = tailored.output_str().to_string();

    println!("{path}");
    println!();
    println!("  core methods installed, whole core : {full_symbols}");
    println!("  core methods installed, tailored   : {tailored_symbols}");
    println!(
        "  left out                           : {}  ({:.0}%)",
        full_symbols.saturating_sub(tailored_symbols),
        full_symbols.saturating_sub(tailored_symbols) as f64 * 100.0 / full_symbols.max(1) as f64
    );
    println!();
    println!();
    let (full_sigs, full_tables) = core_tables(&full);
    let (thin_sigs, thin_tables) = core_tables(&tailored);
    println!("  object heap  : {full_resident} B whole core, {tailored_resident} B tailored");
    println!("  signatures   : {full_sigs} B whole core, {thin_sigs} B tailored");
    println!("  method tables: {full_tables} B whole core, {thin_tables} B tailored");
    let before = full_resident + full_sigs + full_tables;
    let after = tailored_resident + thin_sigs + thin_tables;
    println!(
        "  TOTAL        : {before} B -> {after} B, saved {} B ({:.0}%)",
        before.saturating_sub(after),
        before.saturating_sub(after) as f64 * 100.0 / before.max(1) as f64
    );
    println!();
    // A benchmark times itself, so the `elapsed:` line differs between two
    // runs of anything. What must match is every other line.
    let answers = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| !line.starts_with("elapsed:"))
            .map(str::to_string)
            .collect()
    };
    println!(
        "  same answers: {}",
        if answers(&full_output) == answers(&tailored_output) {
            "yes"
        } else {
            "NO -- the tailored VM answered differently"
        }
    );
    print!("{tailored_output}");
}

/// What the core costs in the VM's own tables, which is where it lives.
///
/// `Heap::bytes` counts *objects*; a bound method is not one. It is an
/// interned signature in the symbol table, a `u32` in every class's method
/// table -- `inherit_methods` copies a parent's table down, so each table is
/// as long as the highest symbol any ancestor defines -- and a function
/// pointer in the VM's primitive list. Sizes are given for a 32-bit part,
/// which is what the figure is for.
fn core_tables(vm: &wren::Vm) -> (usize, usize) {
    let mut signatures = 0;
    let mut count = 0;
    while let Some(name) = vm.method_names.name(count) {
        // A `String` on a 32-bit target is three words plus its bytes.
        signatures += name.len() + 12;
        count += 1;
    }
    let mut tables = 0;
    for id in vm.heap.ids() {
        if let Some(class) = vm.heap.class(id) {
            tables += class.methods.len() * 4;
        }
    }
    (signatures, tables)
}

fn count_symbols(vm: &wren::Vm) -> usize {
    let mut count = 0;
    while vm.method_names.name(count).is_some() {
        count += 1;
    }
    count
}
