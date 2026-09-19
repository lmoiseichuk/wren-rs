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

use wren::bytecode::{disassemble, Chunk, Op};

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
fn the_disassembler_advances_by_the_declared_length() {
    for op in EVERY_OPCODE {
        let (chunk, len) = probe(*op);
        let text = disassemble(&chunk);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "{op:?}: one instruction and a sentinel should disassemble to two lines, got:\n{text}"
        );
        let at: usize = lines[1][..4].trim().parse().expect("an offset");
        assert_eq!(
            at, len,
            "{op:?}: instruction_len says {len}, the disassembler moved to {at}"
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
