//! The instruction set, and the compiled form of a chunk of code.
//!
//! **Byte-packed, as upstream.** The tempting alternative in Rust is a
//! `Vec<Op>` of a rich enum, which is far nicer to write and decode. It is also
//! four bytes per instruction where most instructions here are one or three,
//! and step 4 of the plan — compiling on a host and shipping bytecode to a part
//! that cannot afford a compiler — needs a serialised byte format in any case.
//! So the byte stream is the representation, and decoding is a `match` on a
//! `u8`.
//!
//! The opcodes are upstream's, in upstream's order, minus the ones for
//! features this does not have yet. Keeping the order means a disassembly can
//! be compared against upstream's without a mapping table.

extern crate alloc;

use alloc::vec::Vec;

use crate::value::Value;

/// One instruction.
///
/// The numbering is upstream's `wren_opcodes.h` order. Gaps where upstream has
/// opcodes this does not implement are deliberate: closing them would make the
/// two impossible to read side by side, and the byte values are free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Op {
    /// Push `constants[operand]`. Operand: `u16`.
    Constant = 0,
    /// Push `null`.
    Null = 1,
    /// Push `false`.
    False = 2,
    /// Push `true`.
    True = 3,
    /// Push the local in slot `operand`. Operand: `u8`.
    LoadLocal = 4,
    /// Store the top of the stack into slot `operand`, leaving it. Operand: `u8`.
    StoreLocal = 5,
    /// Push module variable `operand`. Operand: `u16`.
    LoadModuleVar = 6,
    /// Store the top of the stack into module variable `operand`, leaving it.
    /// Operand: `u16`.
    StoreModuleVar = 7,
    /// Discard the top of the stack.
    Pop = 8,
    /// Invoke a method. Operands: arity `u8`, then symbol `u16`.
    ///
    /// The receiver sits below the arguments, so a call of arity *n* consumes
    /// *n + 1* stack slots and leaves one.
    ///
    /// **Upstream splits this into `CALL_0` through `CALL_16`**, so that the
    /// arity is in the opcode rather than an operand — one fewer byte to fetch
    /// on the hottest path in the interpreter. That is a worthwhile
    /// optimisation and it is deliberately not done yet: it multiplies the
    /// dispatch table by seventeen before there is anything to measure the
    /// benefit against.
    Call = 9,
    /// Jump forward by `operand` bytes. Operand: `u16`.
    Jump = 10,
    /// Jump *backward* by `operand` bytes. Operand: `u16`.
    Loop = 11,
    /// Pop, and jump forward by `operand` bytes if it was falsy. Operand: `u16`.
    JumpIf = 12,
    /// If the top of the stack is falsy, jump forward by `operand` and leave
    /// it; otherwise pop and continue. Operand: `u16`.
    And = 13,
    /// The mirror of [`Op::And`]. Operand: `u16`.
    Or = 14,
    /// Return the top of the stack from the current call.
    Return = 15,
    /// End of the compiled chunk. A well-formed chunk always ends with this,
    /// so the interpreter needs no bounds check on the instruction pointer.
    End = 16,
}

impl Op {
    /// Decode a byte, or `None` if it is not an opcode.
    ///
    /// A `match` rather than a transmute — which is what keeps this crate free
    /// of `unsafe`, and costs a jump table the compiler would emit anyway.
    pub fn from_byte(byte: u8) -> Option<Op> {
        let op = match byte {
            0 => Op::Constant,
            1 => Op::Null,
            2 => Op::False,
            3 => Op::True,
            4 => Op::LoadLocal,
            5 => Op::StoreLocal,
            6 => Op::LoadModuleVar,
            7 => Op::StoreModuleVar,
            8 => Op::Pop,
            9 => Op::Call,
            10 => Op::Jump,
            11 => Op::Loop,
            12 => Op::JumpIf,
            13 => Op::And,
            14 => Op::Or,
            15 => Op::Return,
            16 => Op::End,
            _ => return None,
        };
        Some(op)
    }
}

/// A compiled chunk of code: the instructions, and the constants they refer to.
pub struct Chunk {
    pub code: Vec<u8>,
    pub constants: Vec<Value>,
    /// The source line each **byte** of `code` came from, for error messages.
    ///
    /// This is upstream's layout and it is frankly wasteful — a line number per
    /// byte, where a whole statement usually shares one. A run-length encoding
    /// would cost a fraction of it. Left alone until there is a program big
    /// enough for the difference to show up in a measurement.
    pub lines: Vec<u16>,
}

impl Chunk {
    pub fn new() -> Chunk {
        Chunk { code: Vec::new(), constants: Vec::new(), lines: Vec::new() }
    }

    pub fn emit_op(&mut self, op: Op, line: u16) {
        self.emit_byte(op as u8, line);
    }

    pub fn emit_byte(&mut self, byte: u8, line: u16) {
        self.code.push(byte);
        self.lines.push(line);
    }

    /// Emit a big-endian `u16` operand.
    ///
    /// Big-endian because upstream is, and a disassembly that disagrees about
    /// byte order with the reference implementation is a needless puzzle.
    pub fn emit_short(&mut self, value: u16, line: u16) {
        self.emit_byte((value >> 8) as u8, line);
        self.emit_byte((value & 0xff) as u8, line);
    }

    /// Add a constant and return its index, reusing an identical one.
    ///
    /// The reuse matters more here than on a desktop: a loop body mentioning
    /// `1` twenty times should not put twenty copies of it in the constant
    /// table of a part with 8 KB of RAM.
    pub fn add_constant(&mut self, value: Value) -> u16 {
        for (index, existing) in self.constants.iter().enumerate() {
            if existing.is_same(value) {
                return index as u16;
            }
        }
        self.constants.push(value);
        (self.constants.len() - 1) as u16
    }

    /// Emit a jump with a placeholder offset, returning where to patch it.
    pub fn emit_jump(&mut self, op: Op, line: u16) -> usize {
        self.emit_op(op, line);
        self.emit_short(u16::MAX, line);
        self.code.len() - 2
    }

    /// Fill in a jump emitted earlier, now that its destination is known.
    ///
    /// Returns `false` if the body was too long to jump over, which the
    /// compiler turns into an error rather than a silently wrong offset.
    #[must_use]
    pub fn patch_jump(&mut self, at: usize) -> bool {
        // The offset is measured from the instruction after the operand, which
        // is where the instruction pointer will be when the jump executes.
        let distance = self.code.len() - at - 2;
        if distance > u16::MAX as usize {
            return false;
        }
        self.code[at] = (distance >> 8) as u8;
        self.code[at + 1] = (distance & 0xff) as u8;
        true
    }

    /// Emit a backward jump to `start`.
    #[must_use]
    pub fn emit_loop(&mut self, start: usize, line: u16) -> bool {
        self.emit_op(Op::Loop, line);
        let distance = self.code.len() - start + 2;
        if distance > u16::MAX as usize {
            return false;
        }
        self.emit_short(distance as u16, line);
        true
    }

    /// Read a big-endian `u16` at `offset`.
    pub fn read_short(&self, offset: usize) -> u16 {
        ((self.code[offset] as u16) << 8) | self.code[offset + 1] as u16
    }

    /// The source line for the instruction at `offset`, for an error message.
    pub fn line_at(&self, offset: usize) -> u16 {
        self.lines.get(offset).copied().unwrap_or(0)
    }
}

impl Default for Chunk {
    fn default() -> Chunk {
        Chunk::new()
    }
}
