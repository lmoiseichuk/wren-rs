//! Compile `.wren` to `.wrenc`.
//!
//!     cargo run -p wren --example wrenc -- <in.wren> <out.wrenc>
//!     cargo run -p wren --example wrenc -- --check <in.wren> <out.wrenc>
//!     cargo run -p wren --example wrenc -- --strip-lines <in.wren> <out.wrenc>
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
    let check = arguments.iter().any(|argument| argument == "--check");
    let strip = arguments.iter().any(|argument| argument == "--strip-lines");
    // Options anywhere, paths in order, so that `--strip-lines` can be added
    // to an existing command without minding where.
    let rest: Vec<&String> = arguments
        .iter()
        .filter(|argument| !argument.starts_with("--"))
        .collect();

    let (Some(input), Some(output)) = (rest.first(), rest.get(1)) else {
        eprintln!("usage: wrenc [--check] [--strip-lines] <in.wren> <out.wrenc>");
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

    // **A build that ships need not carry a map back to its source.** The
    // line table is flash, the RAM it is read into, and the one part of the
    // file that says which source line each instruction came from.
    let lines = match strip {
        true => wren::wrenc::Lines::Strip,
        false => wren::wrenc::Lines::Keep,
    };
    let bytes = match wren::wrenc::write_with(&vm, &chunk, &source, lines) {
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
