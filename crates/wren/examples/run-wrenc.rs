//! Run a `.wrenc`, with no compiler involved.
//!
//!     cargo run -p wren --example run-wrenc -- <file.wrenc> [file.wren]
//!
//! Given the source as well, it checks the digest first -- which is what the
//! stamp is for: running bytecode that does not match the source somebody
//! thinks they are running is a debugging session nobody enjoys.

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = arguments.first() else {
        eprintln!("usage: run-wrenc <file.wrenc> [file.wren]");
        std::process::exit(2);
    };

    let bytes = std::fs::read(path.as_str()).unwrap_or_else(|error| {
        eprintln!("cannot read {path}: {error}");
        std::process::exit(1);
    });

    let mut vm = wren::Vm::new();
    let loaded = match wren::wrenc::load(&mut vm, &bytes) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("{path}: {}", error.message());
            std::process::exit(1);
        }
    };

    if let Some(source_path) = arguments.get(1) {
        let source = std::fs::read(source_path.as_str()).unwrap_or_default();
        if wren::sha256::digest(&source) != loaded.source_digest {
            eprintln!("{path} was not compiled from {source_path}");
            std::process::exit(1);
        }
    }

    let started = std::time::Instant::now();
    match vm.run_closure(loaded.closure) {
        Ok(()) => {
            print!("{}", vm.output_str());
            eprintln!("[{} ms]", started.elapsed().as_millis());
        }
        Err(error) => {
            print!("{}", vm.output_str());
            eprintln!("line {}: {}", error.line, error.message);
            std::process::exit(1);
        }
    }
}
