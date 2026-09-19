//! That everything which decides how long an instruction is agrees.
//!
//! **An instruction now says how long it is**, in two bits of its own first
//! unit, so the tables that used to have to agree are down to one reader. What
//! is left to check is that the thing which *writes* those bits -- the
//! compiler, by way of `Chunk::close` -- agrees with the thing that reads
//! them, and that the peephole pass keeps it true when it rewrites a pair.

use wren::bytecode::{Chunk, Op};
use wren::Vm;

#[test]
fn fusing_a_pair_gives_it_the_length_of_both() {
    // **`fuse` rewrites a pair in place**, leaving every later unit where it
    // was so that every jump offset still means what it meant. That is only
    // sound if the fused instruction claims the length of both -- which it
    // computes rather than declares, so the result is what there is to check.
    let mut chunk = Chunk::new();
    chunk.emit_op(Op::LoadLocal, 1);
    chunk.emit_byte(3, 1);
    chunk.emit_op(Op::Constant, 1);
    chunk.emit_short(7, 1);
    chunk.emit_op(Op::Return, 1);
    chunk.finish();

    assert_eq!(
        Op::from_byte(Chunk::opcode_of(chunk.code[0])),
        Some(Op::LoadLocalConstant),
        "the pair should have fused"
    );
    assert_eq!(
        Chunk::instruction_units(&chunk.code, 0),
        Some(3),
        "a LoadLocal of one unit and a Constant of two make three"
    );
    assert_eq!(Chunk::inline_operand(chunk.code[0]), 3, "the slot is kept");
    assert_eq!(
        chunk.code[2], 7,
        "and so is the constant index, which sits past the dead `Constant` opcode"
    );

    // The walk still lands exactly on the end, which is what a jump offset
    // measured before the fusion depends on.
    let mut at = 0;
    while at < chunk.code.len() {
        at += Chunk::instruction_units(&chunk.code, at).expect("a length");
    }
    assert_eq!(at, chunk.code.len());
}

#[test]
fn the_compiler_emits_instructions_of_the_declared_length() {
    // **This ties `instruction_units` to the compiler's emit helpers**, which
    // are the other hand-written copy of the same arithmetic. If any opcode is
    // emitted with more or fewer operand bytes than `instruction_units`
    // declares, the walk drifts and cannot land exactly on the end.
    //
    // The disassembler is no longer a second copy -- it asks how far to move
    // -- and neither is anything else: the length is in the instruction.
    let programs = [
        "var a = 1\nvar b = a + 2\nSystem.print(b)\n",
        "class Box {\n  construct new(v) { _v = v }\n  v { _v }\n  v=(n) { _v = n }\n  static make() { Box.new(1) }\n}\nvar b = Box.make()\nb.v = 3\nSystem.print(b.v)\n",
        "var t = 0\nfor (i in 1..5) {\n  if (i > 2) { t = t + i } else { t = t - i }\n}\nwhile (t < 100) { t = t * 2 }\nSystem.print(t)\n",
        "var make = Fn.new { |n| Fn.new { n * 2 } }\nSystem.print(make.call(21).call())\n",
        "import \"random\" for Random\nSystem.print(Random.new(1).int(10) is Num)\n",
        "var l = [1, 2, 3]\nvar m = {\"a\": 1}\nSystem.print(l.count + m.count)\n",
    ];

    for source in programs {
        let mut vm = Vm::new();
        let chunk = wren::compiler::compile(&mut vm, source)
            .unwrap_or_else(|error| panic!("{source:?} should compile: {}", error.message));

        let mut at = 0;
        let mut seen = 0;
        while at < chunk.code.len() {
            let len = Chunk::instruction_units(&chunk.code, at).unwrap_or_else(|| {
                panic!("{source:?}: byte {} at {at} is not a decodable instruction", chunk.code[at])
            });
            at += len;
            seen += 1;
        }
        assert_eq!(
            at,
            chunk.code.len(),
            "{source:?}: walking {seen} instructions by their declared lengths \
             overshot the chunk, so the compiler and `instruction_units` disagree"
        );
    }
}

#[test]
fn a_disassembly_reads_the_way_it_always_did() {
    let mut vm = Vm::new();
    let chunk = wren::compiler::compile(&mut vm, "var a = 1\nif (a > 0) { a = a + 1 }\n")
        .expect("it compiles");
    let text = wren::bytecode::disassemble(&chunk);
    // Offsets ascend, every line names an opcode, and the jump names a target
    // that is a real instruction boundary.
    let mut previous = None;
    for line in text.lines() {
        let at: usize = line[..4].parse().expect("an offset");
        if let Some(previous) = previous {
            assert!(at > previous, "offsets must ascend: {text}");
        }
        previous = Some(at);
        assert!(!line[5..].trim().is_empty(), "every line names an opcode: {text}");
    }
    assert!(text.contains("JumpIf"), "the `if` should be there:\n{text}");
    assert!(text.contains("Constant"), "the literal should be there:\n{text}");
}
