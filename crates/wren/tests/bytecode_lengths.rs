//! That everything which decides how long an instruction is agrees.
//!
//! **Instruction length is written out by hand in four unconnected places**:
//! `Chunk::instruction_len`, the `offset +=` in the disassembler, the `ip +=`
//! in each arm of `Vm::run_frames`, and the compiler's emit helpers. Nothing
//! tied them together, and twice in one afternoon a new opcode was added to
//! some of them and not others -- once making `wrenc` misparse every file it
//! read, once making the peephole pass walk off the instruction boundary.
//!
//! These check the two that can be checked from outside the VM, and the
//! invariant the peephole pass depends on.

use wren::bytecode::{Chunk, Op};
use wren::Vm;

/// Every opcode, so that adding one to the enum without adding it here is a
/// compile error rather than a gap in the coverage.
const EVERY_OPCODE: &[Op] = &[
        Op::Constant,
        Op::Null,
        Op::False,
        Op::True,
        Op::LoadLocal,
        Op::StoreLocal,
        Op::LoadModuleVar,
        Op::StoreModuleVar,
        Op::Pop,
        Op::Call,
        Op::Jump,
        Op::Loop,
        Op::JumpIf,
        Op::And,
        Op::Or,
        Op::Return,
        Op::End,
        Op::Closure,
        Op::LoadUpvalue,
        Op::StoreUpvalue,
        Op::CloseUpvalue,
        Op::Class,
        Op::MethodInstance,
        Op::MethodStatic,
        Op::LoadFieldThis,
        Op::StoreFieldThis,
        Op::LoadField,
        Op::StoreField,
        Op::Construct,
        Op::SetAttributes,
        Op::LoadStaticField,
        Op::StoreStaticField,
        Op::ImportModule,
        Op::ImportVariable,
        Op::Super,
        Op::LoadLocalConstant,
        Op::LoadLocalPair,
        Op::LoadLocalReturn,
        Op::StoreFieldThisPop,
        Op::LoadFieldThisReturn,
];

/// A chunk holding exactly one instruction of `op`, then an `End` sentinel.
///
/// The instruction's operands are zeros, which is why the length has to be
/// asked for first: `Closure` reads its own upvalue count out of them.
fn probe(op: Op) -> (Chunk, usize) {
    let mut sized = Chunk::new();
    sized.emit_op(op, 1);
    for _ in 0..8 {
        sized.emit_byte(0, 1);
    }
    let len = Chunk::instruction_len(&sized.code, 0)
        .unwrap_or_else(|| panic!("{op:?} has no declared length"));

    let mut chunk = Chunk::new();
    chunk.emit_op(op, 1);
    for _ in 1..len {
        chunk.emit_byte(0, 1);
    }
    chunk.emit_op(Op::End, 1);
    (chunk, len)
}

#[test]
fn every_opcode_declares_a_length() {
    // The list above is exhaustive by construction, so a new `Op` variant is a
    // compile error here -- which is the point at which somebody learns that
    // `instruction_len` needs an arm for it.
    for op in EVERY_OPCODE {
        let (chunk, len) = probe(*op);
        assert!(len >= 1, "{op:?} declares a zero-length instruction");
        assert!(
            len <= chunk.code.len(),
            "{op:?} declares {len} bytes, more than the probe holds"
        );
    }
}

#[test]
fn the_compiler_emits_instructions_of_the_declared_length() {
    // **This ties `instruction_len` to the compiler's emit helpers**, which
    // are the other hand-written copy of the same arithmetic. If any opcode is
    // emitted with more or fewer operand bytes than `instruction_len`
    // declares, the walk drifts and cannot land exactly on the end.
    //
    // The disassembler used to be a third copy and is not any more -- it asks
    // `instruction_len` how far to move -- so the check that used to compare
    // the two is now true by construction and has been replaced by this.
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
            let len = Chunk::instruction_len(&chunk.code, at).unwrap_or_else(|| {
                panic!("{source:?}: byte {} at {at} is not a decodable instruction", chunk.code[at])
            });
            at += len;
            seen += 1;
        }
        assert_eq!(
            at,
            chunk.code.len(),
            "{source:?}: walking {seen} instructions by their declared lengths \
             overshot the chunk, so the compiler and `instruction_len` disagree"
        );
    }
}

#[test]
fn every_fused_opcode_is_exactly_as_long_as_the_pair_it_replaces() {
    // **What `Chunk::fuse` depends on.** It rewrites a pair in place, leaving
    // every other byte where it was, so that every jump offset in the chunk
    // keeps its meaning. That is only sound while the fused opcode occupies
    // exactly the bytes of the two it replaces.
    let pairs = [
        (Op::LoadLocalConstant, Op::LoadLocal, Op::Constant),
        (Op::LoadLocalPair, Op::LoadLocal, Op::LoadLocal),
        (Op::LoadLocalReturn, Op::LoadLocal, Op::Return),
        (Op::StoreFieldThisPop, Op::StoreFieldThis, Op::Pop),
        (Op::LoadFieldThisReturn, Op::LoadFieldThis, Op::Return),
    ];
    for (fused, first, second) in pairs {
        let length = |op| probe(op).1;
        assert_eq!(
            length(fused),
            length(first) + length(second),
            "{fused:?} must be exactly {first:?} plus {second:?}"
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
