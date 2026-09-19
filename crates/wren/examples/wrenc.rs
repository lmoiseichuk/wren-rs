//! Compile `.wren` to `.wrenc`.
//!
//!     cargo run -p wren --example wrenc -- <in.wren> <out.wrenc>
//!     cargo run -p wren --example wrenc -- --check <in.wren> <out.wrenc>
//!
//! **A build step, not a runtime one.** The point of the format is that the
//! device never compiles; this is what does the compiling, on a machine that
//! can afford to.
//!
//! `--check` reports whether an existing `.wrenc` was produced from the
//! current source, by comparing the digest the file carries. That is what lets
//! a build skip work rather than recompile everything, and what lets a device
//! refuse bytecode that does not match the source someone thinks it is
//! running.

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let check = arguments.first().map(String::as_str) == Some("--check");
    let rest: Vec<&String> = arguments.iter().skip(usize::from(check)).collect();

    let (Some(input), Some(output)) = (rest.first(), rest.get(1)) else {
        eprintln!("usage: wrenc [--check] <in.wren> <out.wrenc>");
        std::process::exit(2);
    };

    let source = match std::fs::read(input.as_str()) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("cannot read {input}: {error}");
            std::process::exit(1);
        }
    };

    if check {
        let current = wren::sha256::digest(&source);
        match std::fs::read(output.as_str()) {
            Ok(existing) => {
                let mut vm = wren::Vm::new();
                match wren::wrenc::load(&mut vm, &existing) {
                    Ok(loaded) if loaded.source_digest == current => {
                        println!("current: {output}");
                        return;
                    }
                    Ok(_) => println!("stale: {output}"),
                    Err(error) => println!("unreadable: {output} -- {}", error.message()),
                }
            }
            Err(_) => println!("missing: {output}"),
        }
        std::process::exit(1);
    }

    let text = String::from_utf8_lossy(&source).into_owned();
    let mut vm = wren::Vm::new();
    let chunk = match wren::compiler::compile(&mut vm, &text) {
        Ok(chunk) => chunk,
        Err(error) => {
            eprintln!("{input}:{}: {}", error.line, error.message);
            std::process::exit(1);
        }
    };

    let bytes = match wren::wrenc::write(&vm, &chunk, &source) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("{input}: {}", error.message());
            std::process::exit(1);
        }
    };
    if let Err(error) = std::fs::write(output.as_str(), &bytes) {
        eprintln!("cannot write {output}: {error}");
        std::process::exit(1);
    }

    println!(
        "{input} -> {output}  {} B source, {} B bytecode, sha256 {}",
        source.len(),
        bytes.len(),
        &wren::sha256::to_hex(&wren::sha256::digest(&source))[..16],
    );
}
