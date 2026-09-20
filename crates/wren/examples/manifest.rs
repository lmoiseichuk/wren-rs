//! What a compiled program asks of the core library, and what it does not.
//!
//!     cargo run -p wren --release --example manifest -- benchmarks/wrenc/fib.wrenc
//!
//! **Every `.wrenc` already carries this.** The loader has to remap method
//! signatures and module variables into the VM it lands in, so the file names
//! every one of them. Read back, that list is the exact set of core methods a
//! program can possibly reach — and the complement is what a firmware built
//! for that program would never have to link.
//!
//! What the report is for is the decision `core::install` currently leaves to
//! a cargo feature chosen by hand: which classes to build in. The program
//! already said.

fn main() {
    let path = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: manifest <file.wrenc>");
            std::process::exit(2);
        }
    };
    let bytes = std::fs::read(&path).unwrap_or_else(|error| {
        eprintln!("cannot read {path}: {error}");
        std::process::exit(1);
    });
    let manifest = match wren::wrenc::manifest(&bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("{path}: {}", error.message());
            std::process::exit(1);
        }
    };

    // What the core offers, from a VM that has just been built: every
    // signature the core library interned, before any program ran.
    let vm = wren::Vm::new();
    let mut offered: Vec<String> = Vec::new();
    let mut index = 0;
    while let Some(name) = vm.method_names.name(index) {
        offered.push(name.to_string());
        index += 1;
    }

    let used: std::collections::BTreeSet<&str> =
        manifest.signatures.iter().map(String::as_str).collect();

    println!("{path}");
    println!();
    println!("  core interns      {:>5} method signatures", offered.len());
    println!("  this program uses {:>5}", manifest.signatures.len());
    println!(
        "  never reached     {:>5}  ({:.0}% of the core's methods)",
        offered.len().saturating_sub(used.len()),
        offered.len().saturating_sub(used.len()) as f64 * 100.0 / offered.len().max(1) as f64
    );
    println!();
    println!("  classes named: {}", manifest.variables.join(", "));
    println!();
    println!("  methods called:");
    let mut sorted: Vec<&str> = used.iter().copied().collect();
    sorted.sort_unstable();
    for name in sorted {
        println!("    {name}");
    }
}
