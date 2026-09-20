//! Compare `Chunk::max_stack` against what the interpreter actually does.
//!
//!     cargo run --example slots -- 'var f = Fn.new { |a,b,c| a + b + c }'
use wren::bytecode::{disassemble, Chunk};

fn main() {
    let source = std::env::args().nth(1).unwrap_or_else(|| {
        "var f = Fn.new { |a, b, c| a + b + c }\nSystem.print(f.call(1, 2, 3))\n".to_string()
    });
    let mut vm = wren::Vm::new();
    match vm.interpret(&source) {
        Ok(()) => print!("{}", vm.output_str()),
        Err(error) => println!("error: {error:?}"),
    }
    // Every function the program left in the heap, with its computed bound.
    for id in vm.heap.ids() {
        let Some(function) = vm.heap.function(id) else { continue };
        println!(
            "\n--- '{}' arity {} max_stack {:?} ---",
            function.name,
            function.arity,
            Chunk::max_stack(&function.chunk.code)
        );
        print!("{}", disassemble(&function.chunk));
    }
}
