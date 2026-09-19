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
    Closure = 17,
    /// Push the closure's upvalue `operand`. Operand: `u8`.
    LoadUpvalue = 18,
    /// Store the top of the stack into upvalue `operand`, leaving it. `u8`.
    StoreUpvalue = 19,
    /// Close every upvalue at or above the top of the stack, then pop.
    CloseUpvalue = 20,

    /// Make a class. Pops the superclass, then the name below it, and pushes
    /// the class. Operand: `u8`, the number of fields it declares.
    Class = 21,
    /// Bind the closure below the class on the stack as an instance method.
    /// Operand: `u16` symbol. Pops both.
    MethodInstance = 22,
    /// The same, on the metaclass. Operand: `u16` symbol.
    MethodStatic = 23,

    /// Push field `operand` of `this`. Operand: `u8`.
    LoadFieldThis = 24,
    /// Store the top of the stack into field `operand` of `this`. `u8`.
    StoreFieldThis = 25,
    /// Pop an instance and push its field `operand`. Operand: `u8`.
    LoadField = 26,
    /// Pop an instance and store into its field `operand`. Operand: `u8`.
    StoreField = 27,

    /// Replace the class in slot zero with a new instance of it.
    ///
    /// This is how `Foo.new(...)` works: the compiler generates a static
    /// method on the metaclass whose body is this, then a call to the
    /// constructor body, then a return.
    Construct = 28,

    /// Attach attributes to a class. Pops the attributes, then the class.
    SetAttributes = 34,
    /// Push static field `operand` of the class this method belongs to.
    /// Operand: `u8`.
    LoadStaticField = 32,
    /// Store the top of the stack into static field `operand`. Operand: `u8`.
    StoreStaticField = 33,

    /// Load and run the module named by `constants[operand]`, leaving its
    /// result. Operand: `u16`.
    ///
    /// A module already loaded is not run again -- that is what makes two
    /// files importing the same third one share it rather than each get their
    /// own copy of its variables.
    ImportModule = 30,
    /// Push a variable out of an already-imported module. Operands: `u16` for
    /// the module name, then `u16` for the variable name.
    ///
    /// **The module name is repeated rather than remembered from the preceding
    /// `ImportModule`.** A module body may itself import, which would overwrite
    /// any "last imported" state before the outer import got to read its
    /// variables. Naming it twice costs two bytes and removes the ordering
    /// hazard entirely.
    ImportVariable = 31,

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
    Super = 29,

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
    LoadLocalConstant = 35,
    /// `LoadLocal` twice. Operands: `u8` slot, a dead byte, `u8` slot.
    LoadLocalPair = 36,
    /// `LoadLocal` then `Return`. Operands: `u8` slot, a dead byte.
    LoadLocalReturn = 37,
    /// `StoreFieldThis` then `Pop`. Operands: `u8` field, a dead byte.
    StoreFieldThisPop = 38,
    /// `LoadFieldThis` then `Return`. Operands: `u8` field, a dead byte.
    LoadFieldThisReturn = 39,
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
            17 => Op::Closure,
            18 => Op::LoadUpvalue,
            19 => Op::StoreUpvalue,
            20 => Op::CloseUpvalue,
            21 => Op::Class,
            22 => Op::MethodInstance,
            23 => Op::MethodStatic,
            24 => Op::LoadFieldThis,
            25 => Op::StoreFieldThis,
            26 => Op::LoadField,
            27 => Op::StoreField,
            28 => Op::Construct,
            29 => Op::Super,
            30 => Op::ImportModule,
            31 => Op::ImportVariable,
            32 => Op::LoadStaticField,
            33 => Op::StoreStaticField,
            34 => Op::SetAttributes,
            35 => Op::LoadLocalConstant,
            36 => Op::LoadLocalPair,
            37 => Op::LoadLocalReturn,
            38 => Op::StoreFieldThisPop,
            39 => Op::LoadFieldThisReturn,
            _ => return None,
        };
        Some(op)
    }
}

/// A compiled chunk of code: the instructions, and the constants they refer to.
#[derive(Debug)]
pub struct Chunk {
    pub code: Vec<u8>,
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
}

impl Chunk {
    pub fn new() -> Chunk {
        Chunk {
            code: Vec::new(),
            constants: Vec::new(),
            lookup: BTreeMap::new(),
            lines: Vec::new(),
        }
    }

    /// Rebuild a chunk from parts that were serialised.
    ///
    /// The constant lookup is rebuilt rather than stored: it is a compile-time
    /// index for reusing constants, and a loaded chunk will never have another
    /// added. Writing it to the file would cost bytes for a table nothing
    /// reads.
    pub fn from_parts(code: Vec<u8>, constants: Vec<Value>, lines: Vec<(u32, u16)>) -> Chunk {
        let mut chunk = Chunk {
            code,
            constants,
            lookup: BTreeMap::new(),
            lines,
        };
        // **Fused on the way in, not only on the way out.** A `.wrenc` written
        // before the fused opcodes existed carries the pairs unfused, and a
        // file written after carries them fused; running the pass here makes
        // both run the same. It is idempotent -- a fused pair no longer
        // matches anything it looks for.
        chunk.fuse();
        chunk
    }

    pub fn emit_op(&mut self, op: Op, line: u16) {
        self.emit_byte(op as u8, line);
    }

    pub fn emit_byte(&mut self, byte: u8, line: u16) {
        match self.lines.last() {
            Some((_, last)) if *last == line => {}
            _ => self.lines.push((self.code.len() as u32, line)),
        }
        self.code.push(byte);
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

    /// How many bytes the instruction at `at` occupies, opcode included.
    ///
    /// `None` when the byte is not an opcode or the instruction runs off the
    /// end, which for a chunk this crate compiled cannot happen.
    pub fn instruction_len(code: &[u8], at: usize) -> Option<usize> {
        let op = Op::from_byte(*code.get(at)?)?;
        let operands = match op {
            Op::Call | Op::Super => 3,
            Op::ImportVariable => 4,
            Op::Closure => 3 + *code.get(at + 3)? as usize * 2,
            Op::Constant
            | Op::LoadModuleVar
            | Op::StoreModuleVar
            | Op::MethodInstance
            | Op::MethodStatic
            | Op::ImportModule
            | Op::Jump
            | Op::Loop
            | Op::JumpIf
            | Op::And
            | Op::Or => 2,
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
            | Op::Class => 1,
            Op::LoadLocalReturn | Op::StoreFieldThisPop | Op::LoadFieldThisReturn => 2,
            Op::LoadLocalPair => 3,
            Op::LoadLocalConstant => 4,
            Op::Null
            | Op::False
            | Op::True
            | Op::Pop
            | Op::CloseUpvalue
            | Op::Construct
            | Op::Return
            | Op::End
            | Op::SetAttributes => 0,
        };
        Some(1 + operands)
    }

    /// Every offset in `code` that some jump can land on.
    ///
    /// **The one thing fusing has to respect.** Rewriting a pair in place
    /// moves nothing, so every offset in the chunk stays correct -- unless a
    /// jump lands on the *second* instruction of the pair, which after fusing
    /// is a byte that is no longer an instruction.
    fn jump_targets(code: &[u8]) -> Vec<bool> {
        let mut targets = alloc::vec![false; code.len() + 1];
        let mut at = 0;
        while at < code.len() {
            let Some(len) = Chunk::instruction_len(code, at) else {
                break;
            };
            if let Some(op) = Op::from_byte(code[at]) {
                let after = at + 3;
                let offset = match op {
                    Op::Jump | Op::JumpIf | Op::And | Op::Or | Op::Loop => {
                        ((code[at + 1] as usize) << 8) | code[at + 2] as usize
                    }
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
        self.fuse();
        self.lookup = BTreeMap::new();
    }

    pub fn fuse(&mut self) {
        let targets = Chunk::jump_targets(&self.code);
        let mut at = 0;
        while at < self.code.len() {
            let Some(first_len) = Chunk::instruction_len(&self.code, at) else {
                return;
            };
            let second = at + first_len;
            let fused = match (Op::from_byte(self.code[at]), self.code.get(second).copied()) {
                (Some(first), Some(byte)) => match (first, Op::from_byte(byte)) {
                    (Op::LoadLocal, Some(Op::Constant)) => Some(Op::LoadLocalConstant),
                    (Op::LoadLocal, Some(Op::LoadLocal)) => Some(Op::LoadLocalPair),
                    (Op::LoadLocal, Some(Op::Return)) => Some(Op::LoadLocalReturn),
                    (Op::StoreFieldThis, Some(Op::Pop)) => Some(Op::StoreFieldThisPop),
                    (Op::LoadFieldThis, Some(Op::Return)) => Some(Op::LoadFieldThisReturn),
                    _ => None,
                },
                _ => None,
            };

            // A jump landing on the second half would land on a byte that is
            // no longer an instruction.
            match fused {
                Some(fused) if !targets.get(second).copied().unwrap_or(true) => {
                    self.code[at] = fused as u8;
                    at = second + Chunk::instruction_len(&self.code, second).unwrap_or(1);
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
    let mut offset = 0;
    while offset < chunk.code.len() {
        let at = offset;
        let Some(op) = Op::from_byte(chunk.code[offset]) else {
            let _ = writeln!(out, "{at:04} ??? {}", chunk.code[offset]);
            offset += 1;
            continue;
        };
        offset += 1;

        let mut operand = alloc::string::String::new();
        match op {
            Op::Constant
            | Op::LoadModuleVar
            | Op::StoreModuleVar
            | Op::MethodInstance
            | Op::MethodStatic
            | Op::ImportModule => {
                let _ = write!(operand, " {}", chunk.read_short(offset));
                offset += 2;
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
                let _ = write!(operand, " {}", chunk.code[offset]);
                offset += 1;
            }
            Op::Jump | Op::JumpIf | Op::And | Op::Or => {
                let target = offset + 2 + chunk.read_short(offset) as usize;
                let _ = write!(operand, " -> {target:04}");
                offset += 2;
            }
            Op::Loop => {
                let target = offset + 2 - chunk.read_short(offset) as usize;
                let _ = write!(operand, " -> {target:04}");
                offset += 2;
            }
            Op::ImportVariable => {
                let _ = write!(
                    operand,
                    " {} {}",
                    chunk.read_short(offset),
                    chunk.read_short(offset + 2)
                );
                offset += 4;
            }
            Op::Call | Op::Super => {
                let _ = write!(
                    operand,
                    " arity {} symbol {}",
                    chunk.code[offset],
                    chunk.read_short(offset + 1)
                );
                offset += 3;
            }
            Op::Closure => {
                let index = chunk.read_short(offset);
                offset += 2;
                let count = chunk.code[offset] as usize;
                offset += 1;
                let _ = write!(operand, " {index} upvalues {count}");
                // The descriptors are part of the instruction, so they have to
                // be consumed or every later offset is wrong -- which is
                // exactly the sort of thing this exists to catch.
                offset += count * 2;
            }
            // The fused pairs, whose second operand sits past the dead byte
            // where the second opcode used to be.
            Op::LoadLocalConstant => {
                let _ = write!(
                    operand,
                    " {} then constant {}",
                    chunk.code[offset],
                    chunk.read_short(offset + 2)
                );
                offset += 4;
            }
            Op::LoadLocalPair => {
                let _ = write!(
                    operand,
                    " {} then {}",
                    chunk.code[offset],
                    chunk.code[offset + 2]
                );
                offset += 3;
            }
            Op::LoadLocalReturn | Op::StoreFieldThisPop | Op::LoadFieldThisReturn => {
                let _ = write!(operand, " {}", chunk.code[offset]);
                offset += 2;
            }
            _ => {}
        }
        let _ = writeln!(out, "{at:04} {op:?}{operand}");
    }
    out
}
