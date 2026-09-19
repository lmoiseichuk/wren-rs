//! Run upstream test files and print the error each one produces.
//!
//!     cargo run -p wren --example error-probe
//!     cargo run -p wren --example error-probe -- limit/too_many_locals ...
//!
//! **For the error tests, where the suite only checks that *an* error
//! happened.** `suite.rs` scores a file with `// expect runtime error:` as a
//! pass on any error at all, which is upstream's own rule -- so a file can
//! pass while failing for entirely the wrong reason. This prints the message,
//! which is the only way to see that "too many locals" is actually being
//! reported as "too many locals" and not as a parse failure three lines
//! earlier.
//!
//! Paths are relative to `vendor/wren/test/` and take no `.wren` suffix.

fn main() {
    // The ones that have been wrong before, as the default set. Every one of
    // these was at some point passing the suite while reporting something
    // else, which is why they are the list worth re-checking by hand.
    const SUSPECTS: &[&str] = &[
        "language/method/name_too_long",
        "limit/variable_name_too_long",
        "limit/too_many_function_parameters",
        "language/number/literal_too_large",
        "language/nonlocal/undefined",
        "language/function/no_newline_before_close",
        "language/list/newline_before_comma",
        "limit/too_many_inherited_fields",
    ];

    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let paths: Vec<&str> = match arguments.is_empty() {
        true => SUSPECTS.to_vec(),
        false => arguments.iter().map(String::as_str).collect(),
    };

    for path in paths {
        let file = format!("vendor/wren/test/{path}.wren");
        let source = match std::fs::read_to_string(&file) {
            Ok(source) => source,
            Err(error) => {
                println!("{path}: cannot read -- {error}");
                continue;
            }
        };

        let mut vm = wren::Vm::new();
        match vm.interpret(&source) {
            // Worth saying loudly: a test that expects an error and gets none
            // is the failure this tool exists to find.
            Ok(()) => println!("{path}: NO ERROR"),
            Err(error) => println!("{path}: {}", error.message()),
        }
    }
}
