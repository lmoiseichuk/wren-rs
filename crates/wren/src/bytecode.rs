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

use alloc::collections::BTreeMap;
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
    Constant = 21,
    /// Push `null`.
    Null = 4,
    /// Push `false`.
    False = 3,
    /// Push `true`.
    True = 8,
    /// Push the local in slot `operand`. Operand: `u8`.
    LoadLocal = 12,
    /// Store the top of the stack into slot `operand`, leaving it. Operand: `u8`.
    StoreLocal = 17,
    /// Push module variable `operand`. Operand: `u16`.
    LoadModuleVar = 27,
    /// Store the top of the stack into module variable `operand`, leaving it.
    /// Operand: `u16`.
    StoreModuleVar = 33,
    /// Discard the top of the stack.
    Pop = 5,
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
    Call = 34,
    /// Jump forward by `operand` bytes. Operand: `u16`.
    Jump = 23,
    /// Jump *backward* by `operand` bytes. Operand: `u16`.
    Loop = 28,
    /// Pop, and jump forward by `operand` bytes if it was falsy. Operand: `u16`.
    JumpIf = 24,
    /// If the top of the stack is falsy, jump forward by `operand` and leave
    /// it; otherwise pop and continue. Operand: `u16`.
    And = 20,
    /// The mirror of [`Op::And`]. Operand: `u16`.
    Or = 31,
    /// Return the top of the stack from the current call.
    Return = 6,
    /// End of the compiled chunk. A well-formed chunk always ends with this,
    /// so the interpreter needs no bounds check on the instruction pointer.
    End = 2,

    /// Make a closure from the function in `constants[operand]`. Operands:
    /// `u16` constant, `u8` upvalue count, then **two bytes per upvalue**: a
    /// flag saying whether it comes from the enclosing function's locals or
    /// from its upvalues, and the index.
    ///
    /// **The count is in the instruction**, where upstream reads it from the
    /// function object. It costs a byte and means the code can be walked
    /// without the heap — which a disassembler needs, and which a bytecode
    /// loader will need when step 4 ships compiled code to a part that has no
    /// compiler.
    ///
    /// The variable-length operand is why closures are made by an instruction
    /// rather than assembled at compile time: only the enclosing function knows
    /// where each captured variable actually is.
    Closure = 39,
    /// Push the closure's upvalue `operand`. Operand: `u8`.
    LoadUpvalue = 14,
    /// Store the top of the stack into upvalue `operand`, leaving it. `u8`.
    StoreUpvalue = 19,
    /// Close every upvalue at or above the top of the stack, then pop.
    CloseUpvalue = 0,

    /// Make a class. Pops the superclass, then the name below it, and pushes
    /// the class. Operand: `u8`, the number of fields it declares.
    Class = 9,
    /// Bind the closure below the class on the stack as an instance method.
    /// Operand: `u16` symbol. Pops both.
    MethodInstance = 29,
    /// The same, on the metaclass. Operand: `u16` symbol.
    MethodStatic = 30,

    /// Push field `operand` of `this`. Operand: `u8`.
    LoadFieldThis = 11,
    /// Store the top of the stack into field `operand` of `this`. `u8`.
    StoreFieldThis = 16,
    /// Pop an instance and push its field `operand`. Operand: `u8`.
    LoadField = 10,
    /// Pop an instance and store into its field `operand`. Operand: `u8`.
    StoreField = 15,

    /// Replace the class in slot zero with a new instance of it.
    ///
    /// This is how `Foo.new(...)` works: the compiler generates a static
    /// method on the metaclass whose body is this, then a call to the
    /// constructor body, then a return.
    Construct = 1,

    /// Attach attributes to a class. Pops the attributes, then the class.
    SetAttributes = 7,
    /// Push static field `operand` of the class this method belongs to.
    /// Operand: `u8`.
    LoadStaticField = 13,
    /// Store the top of the stack into static field `operand`. Operand: `u8`.
    StoreStaticField = 18,

    /// Load and run the module named by `constants[operand]`, leaving its
    /// result. Operand: `u16`.
    ///
    /// A module already loaded is not run again -- that is what makes two
    /// files importing the same third one share it rather than each get their
    /// own copy of its variables.
    ImportModule = 22,
    /// Push a variable out of an already-imported module. Operands: `u16` for
    /// the module name, then `u16` for the variable name.
    ///
    /// **The module name is repeated rather than remembered from the preceding
    /// `ImportModule`.** A module body may itself import, which would overwrite
    /// any "last imported" state before the outer import got to read its
    /// variables. Naming it twice costs two bytes and removes the ordering
    /// hazard entirely.
    ImportVariable = 37,

    /// Invoke a method on `this`, looking it up from the superclass of the
    /// class this method was bound to. Operands: arity `u8`, symbol `u16`.
    ///
    /// **A super call is bound statically**, to the superclass of the class
    /// the method was *written* in — not of the receiver's class. Otherwise a
    /// method inherited two levels down would call itself forever.
    ///
    /// Upstream stores that class in a constant and rewrites the bytecode when
    /// the method is bound. Here it is recorded on the function instead, which
    /// needs no mutation of a chunk shared through an `Rc`.
    Super = 36,

    // **Fused pairs.** An opcode costs about thirty-one machine instructions
    // on an ESP32-C6 and nearly all of that is dispatch rather than work, so
    // running two as one saves a whole dispatch. Which pairs are worth having
    // was counted rather than guessed -- see `Vm::op_pairs` and
    // `doc/wren-rs/profiling.md`.
    //
    // **Each is laid out as the two instructions it replaces, byte for byte.**
    // Only the first opcode byte changes; the second stays where it is and is
    // never executed. That is what makes the pass safe to run on finished
    // code: nothing moves, so every jump offset in the chunk still means what
    // it meant, and a jump that lands *between* the two is the only case the
    // pass has to refuse. The dead byte costs one byte of image per fusion.
    /// `LoadLocal` then `Constant`. Operands: `u8` slot, a dead byte, `u16`
    /// constant. 18% of the adjacent pairs in `fib`.
    LoadLocalConstant = 38,
    /// `LoadLocal` twice. Operands: `u8` slot, a dead byte, `u8` slot.
    LoadLocalPair = 35,
    /// `LoadLocal` then `Return`. Operands: `u8` slot, a dead byte.
    LoadLocalReturn = 26,
    /// `StoreFieldThis` then `Pop`. Operands: `u8` field, a dead byte.
    StoreFieldThisPop = 32,
    /// `LoadFieldThis` then `Return`. Operands: `u8` field, a dead byte.
    LoadFieldThisReturn = 25,
}

/// The opcodes as plain bytes, for matching the instruction stream directly.
///
/// **One jump table rather than two.** The interpreter decoded a byte into an
/// `Op` and then matched on the `Op`: a switch whose result feeds a switch.
/// Matching the byte itself is one table, and the `Option` that carried "not
/// an opcode" between them becomes the `_` arm that was needed anyway.
pub mod code {
    use super::Op;

    pub const CONSTANT: u8 = Op::Constant as u8;
    pub const NULL: u8 = Op::Null as u8;
    pub const FALSE: u8 = Op::False as u8;
    pub const TRUE: u8 = Op::True as u8;
    pub const LOAD_LOCAL: u8 = Op::LoadLocal as u8;
    pub const STORE_LOCAL: u8 = Op::StoreLocal as u8;
    pub const LOAD_MODULE_VAR: u8 = Op::LoadModuleVar as u8;
    pub const STORE_MODULE_VAR: u8 = Op::StoreModuleVar as u8;
    pub const POP: u8 = Op::Pop as u8;
    pub const CALL: u8 = Op::Call as u8;
    pub const JUMP: u8 = Op::Jump as u8;
    pub const LOOP: u8 = Op::Loop as u8;
    pub const JUMP_IF: u8 = Op::JumpIf as u8;
    pub const AND: u8 = Op::And as u8;
    pub const OR: u8 = Op::Or as u8;
    pub const RETURN: u8 = Op::Return as u8;
    pub const END: u8 = Op::End as u8;
    pub const CLOSURE: u8 = Op::Closure as u8;
    pub const LOAD_UPVALUE: u8 = Op::LoadUpvalue as u8;
    pub const STORE_UPVALUE: u8 = Op::StoreUpvalue as u8;
    pub const CLOSE_UPVALUE: u8 = Op::CloseUpvalue as u8;
    pub const CLASS: u8 = Op::Class as u8;
    pub const METHOD_INSTANCE: u8 = Op::MethodInstance as u8;
    pub const METHOD_STATIC: u8 = Op::MethodStatic as u8;
    pub const LOAD_FIELD_THIS: u8 = Op::LoadFieldThis as u8;
    pub const STORE_FIELD_THIS: u8 = Op::StoreFieldThis as u8;
    pub const LOAD_FIELD: u8 = Op::LoadField as u8;
    pub const STORE_FIELD: u8 = Op::StoreField as u8;
    pub const CONSTRUCT: u8 = Op::Construct as u8;
    pub const SET_ATTRIBUTES: u8 = Op::SetAttributes as u8;
    pub const LOAD_STATIC_FIELD: u8 = Op::LoadStaticField as u8;
    pub const STORE_STATIC_FIELD: u8 = Op::StoreStaticField as u8;
    pub const IMPORT_MODULE: u8 = Op::ImportModule as u8;
    pub const IMPORT_VARIABLE: u8 = Op::ImportVariable as u8;
    pub const SUPER: u8 = Op::Super as u8;
    pub const LOAD_LOCAL_CONSTANT: u8 = Op::LoadLocalConstant as u8;
    pub const LOAD_LOCAL_PAIR: u8 = Op::LoadLocalPair as u8;
    pub const LOAD_LOCAL_RETURN: u8 = Op::LoadLocalReturn as u8;
    pub const STORE_FIELD_THIS_POP: u8 = Op::StoreFieldThisPop as u8;
    pub const LOAD_FIELD_THIS_RETURN: u8 = Op::LoadFieldThisReturn as u8;
}

#[cfg(test)]
mod opcode_bytes {
    use super::*;

    /// Every opcode's byte, as an exhaustive match.
    ///
    /// **The point of it is the compile error.** `Vm::run_frames` matches raw
    /// bytes, which costs the exhaustiveness a `match op` gave for free; a new
    /// `Op` variant has to be given an arm here, which is where it learns that
    /// `code` needs a constant and the interpreter needs a case.
    fn byte_of(op: Op) -> u8 {
        match op {
            Op::Constant => code::CONSTANT,
            Op::Null => code::NULL,
            Op::False => code::FALSE,
            Op::True => code::TRUE,
            Op::LoadLocal => code::LOAD_LOCAL,
            Op::StoreLocal => code::STORE_LOCAL,
            Op::LoadModuleVar => code::LOAD_MODULE_VAR,
            Op::StoreModuleVar => code::STORE_MODULE_VAR,
            Op::Pop => code::POP,
            Op::Call => code::CALL,
            Op::Jump => code::JUMP,
            Op::Loop => code::LOOP,
            Op::JumpIf => code::JUMP_IF,
            Op::And => code::AND,
            Op::Or => code::OR,
            Op::Return => code::RETURN,
            Op::End => code::END,
            Op::Closure => code::CLOSURE,
            Op::LoadUpvalue => code::LOAD_UPVALUE,
            Op::StoreUpvalue => code::STORE_UPVALUE,
            Op::CloseUpvalue => code::CLOSE_UPVALUE,
            Op::Class => code::CLASS,
            Op::MethodInstance => code::METHOD_INSTANCE,
            Op::MethodStatic => code::METHOD_STATIC,
            Op::LoadFieldThis => code::LOAD_FIELD_THIS,
            Op::StoreFieldThis => code::STORE_FIELD_THIS,
            Op::LoadField => code::LOAD_FIELD,
            Op::StoreField => code::STORE_FIELD,
            Op::Construct => code::CONSTRUCT,
            Op::SetAttributes => code::SET_ATTRIBUTES,
            Op::LoadStaticField => code::LOAD_STATIC_FIELD,
            Op::StoreStaticField => code::STORE_STATIC_FIELD,
            Op::ImportModule => code::IMPORT_MODULE,
            Op::ImportVariable => code::IMPORT_VARIABLE,
            Op::Super => code::SUPER,
            Op::LoadLocalConstant => code::LOAD_LOCAL_CONSTANT,
            Op::LoadLocalPair => code::LOAD_LOCAL_PAIR,
            Op::LoadLocalReturn => code::LOAD_LOCAL_RETURN,
            Op::StoreFieldThisPop => code::STORE_FIELD_THIS_POP,
            Op::LoadFieldThisReturn => code::LOAD_FIELD_THIS_RETURN,
        }
    }

    #[test]
    fn the_constants_are_the_discriminants() {
        for byte in 0..=u8::MAX {
            if let Some(op) = Op::from_byte(byte) {
                assert_eq!(byte_of(op), byte, "{op:?} decodes from a byte it is not");
            }
        }
    }
}

impl Op {
    /// Decode a byte, or `None` if it is not an opcode.
    ///
    /// **The numbering is in order of instruction length.** That no longer
    /// decides anything -- [`Chunk::instruction_units`] reads the length out
    /// of the instruction's own bits -- but it keeps related opcodes together
    /// and costs nothing to preserve.
    ///
    /// A `match` rather than a transmute — which is what keeps this crate free
    /// of `unsafe`. The interpreter no longer calls it: `run_frames` matches
    /// the byte directly, and this is for the compiler, the disassembler and
    /// the file reader, none of which are hot.
    pub fn from_byte(byte: u8) -> Option<Op> {
        let op = match byte {
            0 => Op::CloseUpvalue,
            1 => Op::Construct,
            2 => Op::End,
            3 => Op::False,
            4 => Op::Null,
            5 => Op::Pop,
            6 => Op::Return,
            7 => Op::SetAttributes,
            8 => Op::True,
            9 => Op::Class,
            10 => Op::LoadField,
            11 => Op::LoadFieldThis,
            12 => Op::LoadLocal,
            13 => Op::LoadStaticField,
            14 => Op::LoadUpvalue,
            15 => Op::StoreField,
            16 => Op::StoreFieldThis,
            17 => Op::StoreLocal,
            18 => Op::StoreStaticField,
            19 => Op::StoreUpvalue,
            20 => Op::And,
            21 => Op::Constant,
            22 => Op::ImportModule,
            23 => Op::Jump,
            24 => Op::JumpIf,
            25 => Op::LoadFieldThisReturn,
            26 => Op::LoadLocalReturn,
            27 => Op::LoadModuleVar,
            28 => Op::Loop,
            29 => Op::MethodInstance,
            30 => Op::MethodStatic,
            31 => Op::Or,
            32 => Op::StoreFieldThisPop,
            33 => Op::StoreModuleVar,
            34 => Op::Call,
            35 => Op::LoadLocalPair,
            36 => Op::Super,
            37 => Op::ImportVariable,
            38 => Op::LoadLocalConstant,
            39 => Op::Closure,
            _ => return None,
        };
        Some(op)

    }
}

/// The furthest a jump may reach, in units.
///
/// **Not what the operand could hold.** A `u16` of units reaches 65,535
/// instructions, which is twice as far as upstream's 65,535 *bytes* -- and a
/// program upstream rejects but this accepts is exactly the kind of difference
/// this implementation exists not to have. Upstream's two limit tests,
/// `jump_too_far` and `loop_too_far`, are built to sit just past its reach;
/// with the wider one they compile, and `loop_too_far` then runs a loop it was
/// never meant to run.
///
/// Half the operand's range is a little under upstream's reach in the worst
/// case and a little over in the best, which is as close as two different
/// encodings come.
pub const MAX_JUMP: usize = u16::MAX as usize / 2;

/// Which bits of an instruction's first unit hold what. See [`Chunk::code`].
///
/// Six bits of opcode is 64, against the 40 there are; two bits of length
/// cover one, two and three units, and the fourth value is the escape for the
/// one instruction whose length is not fixed.
pub const OPCODE_MASK: u16 = 0x3f;
/// Where the length field starts.
pub const LENGTH_SHIFT: u16 = 6;
/// Where the inline `u8` operand starts.
pub const OPERAND_SHIFT: u16 = 8;
/// The length field's escape value: the instruction is two units plus one per
/// unit its inline operand counts. Only `Closure` uses it, for its upvalues,
/// and Wren caps those at what a `u8` holds -- so 257 units at the very worst,
/// against a `Closure` of two units when it captures nothing.
pub const VARIABLE_LENGTH: u16 = 3;

/// A compiled chunk of code: the instructions, and the constants they refer to.
#[derive(Debug)]
pub struct Chunk {
    /// The instructions, one 16-bit unit at a time.
    ///
    /// **A unit, not a byte.** The first unit of an instruction is
    ///
    /// | bits | |
    /// |---|---|
    /// | 0..5 | the opcode, so 64 of them |
    /// | 6..7 | how many units the instruction is, less one |
    /// | 8..15 | an inline `u8` operand, where it has one |
    ///
    /// and any further operands are whole units. A length of `3` in that
    /// field means *variable*, which only `Closure` is: it holds the number of
    /// upvalues in its inline byte and is two units plus one per upvalue.
    ///
    /// **What it buys is the operand read.** A `u16` operand used to be two
    /// bounds-checked byte loads, a shift and an or; it is now one aligned
    /// load. An instruction with a single `u8` operand -- `LoadLocal` and its
    /// kind -- used to be two loads and is now one, because the operand rides
    /// in the opcode's own unit.
    ///
    /// **What it costs is about a fifth of the code**, which on the four
    /// benchmarks is 130 bytes or less: a one-byte instruction rounds up to a
    /// unit, and a three-byte one to two units. Against a heap measured in
    /// hundreds of kilobytes that is not a trade worth thinking about twice.
    pub code: Vec<u16>,
    pub constants: Vec<Value>,
    /// Bit pattern of each constant to its index, so adding one is a lookup
    /// rather than a scan.
    ///
    /// **This is not a micro-optimisation.** Reuse is checked on every single
    /// constant, so a linear scan makes compiling quadratic in the number of
    /// them: upstream's own `limit/many_constants` test declares 65,540 and
    /// took seventeen seconds to compile, against about two billion
    /// comparisons. It is the kind of cost that never shows up on the small
    /// programs one tests with and then makes a real file appear to hang.
    ///
    /// A `BTreeMap` rather than a hash map because `alloc` has one and does
    /// not have the other, and this crate takes no dependencies. `log n` on a
    /// table capped at 65,536 entries is sixteen comparisons at worst.
    lookup: BTreeMap<u64, u16>,
    /// Where each source line's code starts: `(offset, line)`, in order.
    ///
    /// **One entry per line, not per byte.** Upstream's layout is a line
    /// number for every byte of code, which measured at exactly twice the size
    /// of the code itself -- 2,464 bytes of line numbers for 1,232 bytes of
    /// instructions in `binary_trees`. A whole statement shares one line, so
    /// nearly every entry equalled the one before it.
    ///
    /// **The file format already knew this**: `.wrenc` has always stored the
    /// table run-length encoded, and the reader expanded it on the way in.
    /// Only memory was paying, which is the wrong way round for this project.
    ///
    /// Nothing reads it on a working program -- [`Chunk::line_at`] is called
    /// from the interpreter's error paths alone -- so a binary search costs
    /// nothing that matters.
    pub lines: Vec<(u32, u16)>,
    /// Where the instruction now being emitted starts, so that its length can
    /// be written into it when the next one begins. See [`Chunk::close`].
    building: Option<usize>,
    /// Whether the instruction being emitted has already used the inline
    /// operand slot in its first unit. A `0` there is a real operand value,
    /// so the slot's emptiness cannot be read off the bits.
    inline_taken: bool,
}

impl Chunk {
    pub fn new() -> Chunk {
        Chunk {
            code: Vec::new(),
            constants: Vec::new(),
            lookup: BTreeMap::new(),
            lines: Vec::new(),
            building: None,
            inline_taken: false,
        }
    }

    /// Rebuild a chunk from parts that were serialised.
    ///
    /// The constant lookup is rebuilt rather than stored: it is a compile-time
    /// index for reusing constants, and a loaded chunk will never have another
    /// added. Writing it to the file would cost bytes for a table nothing
    /// reads.
    pub fn from_parts(code: Vec<u16>, constants: Vec<Value>, lines: Vec<(u32, u16)>) -> Chunk {
        let mut chunk = Chunk {
            code,
            constants,
            lookup: BTreeMap::new(),
            lines,
            building: None,
            inline_taken: false,
        };
        // **Fused on the way in, not only on the way out.** A `.wrenc` written
        // before the fused opcodes existed carries the pairs unfused, and a
        // file written after carries them fused; running the pass here makes
        // both run the same. It is idempotent -- a fused pair no longer
        // matches anything it looks for.
        chunk.fuse();
        chunk
    }

    /// Begin an instruction. Its length is written when the next one begins.
    pub fn emit_op(&mut self, op: Op, line: u16) {
        self.close();
        match self.lines.last() {
            Some((_, last)) if *last == line => {}
            _ => self.lines.push((self.code.len() as u32, line)),
        }
        self.building = Some(self.code.len());
        self.code.push(op as u16);
    }

    /// Add a `u8` operand: into the opcode's own unit if that slot is still
    /// free, otherwise into a unit of its own.
    pub fn emit_byte(&mut self, byte: u8, line: u16) {
        let _ = line;
        match self.building {
            Some(start) if self.code[start] >> OPERAND_SHIFT == 0 && !self.inline_taken => {
                self.code[start] |= u16::from(byte) << OPERAND_SHIFT;
                self.inline_taken = true;
            }
            _ => self.code.push(u16::from(byte)),
        }
    }

    /// Add two `u8` operands sharing one unit.
    ///
    /// For `Closure`'s upvalue descriptors, which come in pairs and would
    /// otherwise take a unit each.
    pub fn emit_byte_pair(&mut self, low: u8, high: u8, line: u16) {
        let _ = line;
        self.code
            .push(u16::from(low) | (u16::from(high) << OPERAND_SHIFT));
    }

    /// Finish the instruction being emitted by writing its length into it.
    ///
    /// **Lengths are known only in arrears.** An instruction is as long as
    /// what was emitted into it, and that is known when the next one starts --
    /// or when the chunk is finished.
    fn close(&mut self) {
        let Some(start) = self.building.take() else {
            return;
        };
        self.inline_taken = false;
        let units = self.code.len() - start;
        // Three units is the longest fixed instruction, so a longer one can
        // only be `Closure` and is marked variable.
        let bits = match units {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            _ => VARIABLE_LENGTH,
        };
        self.code[start] |= bits << LENGTH_SHIFT;
    }

    /// Emit a big-endian `u16` operand.
    ///
    /// Big-endian because upstream is, and a disassembly that disagrees about
    /// byte order with the reference implementation is a needless puzzle.
    pub fn emit_short(&mut self, value: u16, line: u16) {
        let _ = line;
        self.code.push(value);
    }

    /// Add a constant and return its index, reusing an identical one.
    ///
    /// The reuse matters more here than on a desktop: a loop body mentioning
    /// `1` twenty times should not put twenty copies of it in the constant
    /// table of a part with 8 KB of RAM.
    ///
    /// Returns `None` when the table is full. **The index is a `u16` in the
    /// bytecode**, so 65,536 is a hard ceiling rather than a policy: past it
    /// the operand wraps and the function silently loads the wrong constant.
    /// Upstream has the same limit and the same test for it.
    pub fn add_constant(&mut self, value: Value) -> Option<u16> {
        let bits = value.to_bits();
        if let Some(index) = self.lookup.get(&bits) {
            return Some(*index);
        }
        // Indices run 0..=u16::MAX, so the table is full at 65,536 entries.
        if self.constants.len() > u16::MAX as usize {
            return None;
        }
        let index = self.constants.len() as u16;
        self.constants.push(value);
        self.lookup.insert(bits, index);
        Some(index)
    }

    /// Emit a jump with a placeholder offset, returning where to patch it.
    pub fn emit_jump(&mut self, op: Op, line: u16) -> usize {
        self.emit_op(op, line);
        self.emit_short(u16::MAX, line);
        self.code.len() - 1
    }

    /// Fill in a jump emitted earlier, now that its destination is known.
    ///
    /// Returns `false` if the body was too long to jump over, which the
    /// compiler turns into an error rather than a silently wrong offset.
    #[must_use]
    pub fn patch_jump(&mut self, at: usize) -> bool {
        // The offset is measured from the instruction after the operand, which
        // is where the instruction pointer will be when the jump executes.
        // `at` is the operand's own unit, so the instruction after the jump
        // starts one unit later. Distances are in units now, which is also
        // four times the reach a byte offset had.
        self.close();
        let distance = self.code.len() - at - 1;
        if distance > MAX_JUMP {
            return false;
        }
        self.code[at] = distance as u16;
        true
    }

    /// Emit a backward jump to `start`.
    #[must_use]
    pub fn emit_loop(&mut self, start: usize, line: u16) -> bool {
        self.emit_op(Op::Loop, line);
        // One more unit for the operand about to be emitted: after it the
        // instruction pointer is at `code.len() + 1`, and it has to land on
        // `start`.
        let distance = self.code.len() + 1 - start;
        if distance > MAX_JUMP {
            return false;
        }
        self.emit_short(distance as u16, line);
        true
    }

    /// The operand unit at `at`. One aligned load.
    pub fn read_short(&self, at: usize) -> u16 {
        self.code[at]
    }

    /// Replace the `u8` operand carried inside the instruction at `at`.
    ///
    /// For an operand that is only known once the instruction has been
    /// emitted -- a class's field count, which the class body decides.
    pub fn patch_inline_operand(&mut self, at: usize, value: u8) {
        let keep = self.code[at] & !(0xff << OPERAND_SHIFT);
        self.code[at] = keep | (u16::from(value) << OPERAND_SHIFT);
    }

    /// The `u8` operand carried inside an instruction's first unit.
    #[inline(always)]
    pub fn inline_operand(unit: u16) -> u8 {
        (unit >> OPERAND_SHIFT) as u8
    }

    /// The opcode an instruction's first unit names.
    #[inline(always)]
    pub fn opcode_of(unit: u16) -> u8 {
        (unit & OPCODE_MASK) as u8
    }

    /// How many bytes the instruction at `at` occupies, opcode included.
    ///
    /// `None` when the byte is not an opcode or the instruction runs off the
    /// end, which for a chunk this crate compiled cannot happen.
    /// How many units the instruction at `at` occupies, its first included.
    ///
    /// **The instruction says so itself**, in two bits of its own first unit.
    /// There is no table to keep in step with the compiler, the interpreter
    /// and the disassembler -- which there was, four times over, and twice in
    /// one afternoon they disagreed.
    ///
    /// `None` when `at` is past the end, or when a variable-length
    /// instruction's count runs off it.
    #[inline(always)]
    pub fn instruction_units(code: &[u16], at: usize) -> Option<usize> {
        let unit = *code.get(at)?;
        Some(match (unit >> LENGTH_SHIFT) & 0b11 {
            VARIABLE_LENGTH => 2 + Chunk::inline_operand(unit) as usize,
            fixed => fixed as usize + 1,
        })
    }

    /// Every offset in `code` that some jump can land on.
    ///
    /// **The one thing fusing has to respect.** Rewriting a pair in place
    /// moves nothing, so every offset in the chunk stays correct -- unless a
    /// jump lands on the *second* instruction of the pair, which after fusing
    /// is a byte that is no longer an instruction.
    fn jump_targets(code: &[u16]) -> Vec<bool> {
        let mut targets = alloc::vec![false; code.len() + 1];
        let mut at = 0;
        while at < code.len() {
            let Some(len) = Chunk::instruction_units(code, at) else {
                break;
            };
            if let Some(op) = Op::from_byte(Chunk::opcode_of(code[at])) {
                // **Where a jump is measured from is the end of the
                // instruction**, which is asked for rather than restated. A
                // jump target computed one unit out is the kind of fault that
                // shows up as the wrong code running.
                let after = at + len;
                let offset = match op {
                    Op::Jump | Op::JumpIf | Op::And | Op::Or | Op::Loop => code[at + 1] as usize,
                    _ => 0,
                };
                let target = match op {
                    Op::Jump | Op::JumpIf | Op::And | Op::Or => Some(after + offset),
                    Op::Loop => after.checked_sub(offset),
                    _ => None,
                };
                if let Some(target) = target {
                    if let Some(slot) = targets.get_mut(target) {
                        *slot = true;
                    }
                }
            }
            at += len;
        }
        targets
    }

    /// Replace adjacent pairs of instructions with the single opcode that
    /// does both, where there is one.
    ///
    /// Run once, on a finished chunk. See the note on the fused variants of
    /// [`Op`] for why this can be done in place.
    /// Nothing more will be added to this chunk: fuse its instruction pairs
    /// and let go of what only the compiler needed.
    ///
    /// **The constant index is compile-time state that was outliving the
    /// compiler.** `lookup` exists so that adding a constant can find an equal
    /// one already there, and a chunk that has finished compiling will never
    /// add another -- but it was kept for the life of the program, on a device
    /// where that is the whole of memory. Measured on `binary_trees`: 240
    /// bytes of entries against 1,232 bytes of actual code, and a `BTreeMap`
    /// node is far larger than its entries.
    pub fn finish(&mut self) {
        // **The last instruction has nothing after it to close it.** Every
        // other one is closed when the next begins; without this the final
        // instruction keeps a length of zero-plus-one, and everything that
        // walks the code -- the fusion pass and the jump-target scan it
        // depends on -- misreads the tail.
        self.close();
        self.fuse();
        self.lookup = BTreeMap::new();
    }

    pub fn fuse(&mut self) {
        let targets = Chunk::jump_targets(&self.code);
        let mut at = 0;
        while at < self.code.len() {
            let Some(first_len) = Chunk::instruction_units(&self.code, at) else {
                return;
            };
            let second = at + first_len;
            let fused = match (
                Op::from_byte(Chunk::opcode_of(self.code[at])),
                self.code.get(second).copied(),
            ) {
                (Some(first), Some(unit)) => match (first, Op::from_byte(Chunk::opcode_of(unit))) {
                    (Op::LoadLocal, Some(Op::Constant)) => Some(Op::LoadLocalConstant),
                    (Op::LoadLocal, Some(Op::LoadLocal)) => Some(Op::LoadLocalPair),
                    (Op::LoadLocal, Some(Op::Return)) => Some(Op::LoadLocalReturn),
                    (Op::StoreFieldThis, Some(Op::Pop)) => Some(Op::StoreFieldThisPop),
                    (Op::LoadFieldThis, Some(Op::Return)) => Some(Op::LoadFieldThisReturn),
                    _ => None,
                },
                _ => None,
            };

            // A jump landing on the second half would land on a unit that is
            // no longer an instruction.
            match fused {
                Some(fused) if !targets.get(second).copied().unwrap_or(true) => {
                    let second_len = Chunk::instruction_units(&self.code, second).unwrap_or(1);
                    // **The opcode changes; the length has to change with
                    // it.** A fused instruction spans both of the ones it
                    // replaces, so that nothing moves and every jump offset
                    // in the chunk still means what it meant -- and the
                    // length field is what says so.
                    let units = first_len + second_len;
                    let bits = match units {
                        0 | 1 => 0,
                        2 => 1,
                        3 => 2,
                        _ => VARIABLE_LENGTH,
                    };
                    let keep = self.code[at] & !(OPCODE_MASK | (0b11 << LENGTH_SHIFT));
                    self.code[at] = keep | fused as u16 | (bits << LENGTH_SHIFT);
                    at = second + second_len;
                }
                _ => at += first_len,
            }
        }
    }

    /// What this chunk costs in memory, split into its parts.
    ///
    /// `(code, lines, constants, lookup)`, in bytes. The last two are the
    /// constant table and the compile-time index used to deduplicate it.
    #[cfg(feature = "profile")]
    pub fn footprint(&self) -> (usize, usize, usize, usize) {
        (
            self.code.capacity(),
            self.lines.capacity() * core::mem::size_of::<(u32, u16)>(),
            self.constants.capacity() * core::mem::size_of::<Value>(),
            // A `BTreeMap` node holds up to eleven entries plus its links; this
            // counts the entries alone, so it is a floor rather than the cost.
            self.lookup.len() * (core::mem::size_of::<u64>() + core::mem::size_of::<u16>()),
        )
    }

    /// The source line for the instruction at `offset`, for an error message.
    /// **Never inlined, because every caller is an error path.** There are
    /// eight of them inside the interpreter's dispatch loop, one of them in
    /// the closure that fixes up a failed primitive's line -- and when this
    /// became a search rather than an index, inlining that body eight times
    /// cost 2% of `fib`'s instructions *on the path where nothing fails*: the
    /// closure grew past what the optimiser would fold away, so the successful
    /// call had to build it.
    #[cold]
    #[inline(never)]
    pub fn line_at(&self, offset: usize) -> u16 {
        let offset = offset as u32;
        // The last entry that starts at or before this offset owns it.
        match self.lines.binary_search_by_key(&offset, |(start, _)| *start) {
            Ok(index) => self.lines[index].1,
            Err(0) => 0,
            Err(index) => self.lines[index - 1].1,
        }
    }
}

impl Default for Chunk {
    fn default() -> Chunk {
        Chunk::new()
    }
}

/// Disassemble a chunk, one instruction per line.
///
/// Not used by the VM. It exists because a stack-discipline bug — the
/// compiler's idea of which slot a local is in disagreeing with the VM's — is
/// close to undebuggable from the outside, and obvious the moment the
/// instructions are laid out with their offsets.
#[cfg(feature = "std")]
pub fn disassemble(chunk: &Chunk) -> alloc::string::String {
    use core::fmt::Write as _;

    let mut out = alloc::string::String::new();
    let mut at = 0;
    while at < chunk.code.len() {
        let Some(op) = Op::from_byte(Chunk::opcode_of(chunk.code[at])) else {
            let _ = writeln!(out, "{at:04} ??? {}", chunk.code[at]);
            at += 1;
            continue;
        };
        // **How far to move is asked for, not restated.** This carried its own
        // per-opcode `offset +=` table, a second copy of `instruction_units`
        // that had to agree with it byte for byte and twice did not. The arms
        // below format operands and nothing else; the walk is one call.
        let Some(len) = Chunk::instruction_units(&chunk.code, at) else {
            let _ = writeln!(out, "{at:04} ??? truncated");
            break;
        };
        let offset = at + 1;

        let mut operand = alloc::string::String::new();
        match op {
            Op::Constant
            | Op::LoadModuleVar
            | Op::StoreModuleVar
            | Op::MethodInstance
            | Op::MethodStatic
            | Op::ImportModule => {
                let _ = write!(operand, " {}", chunk.read_short(offset));
            }
            Op::LoadLocal
            | Op::StoreLocal
            | Op::LoadUpvalue
            | Op::StoreUpvalue
            | Op::LoadFieldThis
            | Op::StoreFieldThis
            | Op::LoadField
            | Op::StoreField
            | Op::LoadStaticField
            | Op::StoreStaticField
            | Op::Class => {
                let _ = write!(operand, " {}", Chunk::inline_operand(chunk.code[at]));
            }
            Op::Jump | Op::JumpIf | Op::And | Op::Or => {
                let target = at + len + chunk.code[offset] as usize;
                let _ = write!(operand, " -> {target:04}");
            }
            Op::Loop => {
                let target = at + len - chunk.code[offset] as usize;
                let _ = write!(operand, " -> {target:04}");
            }
            Op::ImportVariable => {
                let _ = write!(
                    operand,
                    " {} {}",
                    chunk.code[offset],
                    chunk.code[offset + 1]
                );
            }
            Op::Call | Op::Super => {
                let _ = write!(
                    operand,
                    " arity {} symbol {}",
                    Chunk::inline_operand(chunk.code[at]),
                    chunk.code[offset]
                );
            }
            Op::Closure => {
                // The count rides inline; the descriptors that follow are one
                // unit each and are counted by `instruction_units`.
                let index = chunk.code[offset];
                let count = Chunk::inline_operand(chunk.code[at]);
                let _ = write!(operand, " {index} upvalues {count}");
            }
            // The fused pairs, whose second operand sits past the dead byte
            // where the second opcode used to be.
            Op::LoadLocalConstant => {
                let _ = write!(
                    operand,
                    " {} then constant {}",
                    Chunk::inline_operand(chunk.code[at]),
                    chunk.code[offset + 1]
                );
            }
            Op::LoadLocalPair => {
                let _ = write!(
                    operand,
                    " {} then {}",
                    Chunk::inline_operand(chunk.code[at]),
                    Chunk::inline_operand(chunk.code[offset])
                );
            }
            Op::LoadLocalReturn | Op::StoreFieldThisPop | Op::LoadFieldThisReturn => {
                let _ = write!(operand, " {}", Chunk::inline_operand(chunk.code[at]));
            }
            _ => {}
        }
        let _ = writeln!(out, "{at:04} {op:?}{operand}");
        at += len;
    }
    out
}
