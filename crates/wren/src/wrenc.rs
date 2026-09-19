//! `.wrenc` — compiled Wren, for a part that cannot afford a compiler.
//!
//! **This is the whole point of step 4.** Upstream spends 33,552 B of stack
//! compiling its own core library before user code runs, and this
//! implementation spends a compiler's worth of flash on the lexer and parser.
//! A CH32V006 has 8 KB of RAM and 62 KB of flash; it cannot have either. What
//! it can have is a byte stream produced on a workstation.
//!
//! # What has to travel, and why it is not just the code
//!
//! A `Chunk` is not self-contained. Two of its operands are indices into
//! tables the VM builds at start-up:
//!
//! * `Op::Call` carries a **method symbol** — an index into the VM's interned
//!   signatures, assigned in the order the compiler first saw each one.
//! * `Op::LoadModuleVar` carries an index into the **module's** variables,
//!   which begins with however many the core library installed.
//!
//! Both depend on what the VM did before compiling, so neither survives being
//! written to a file and read back into a different VM — a core library with
//! one more method in it would shift every symbol. So the file carries the
//! *names*, and loading rewrites the operands against the names the loading VM
//! actually has. That costs a pass over the code at load time and makes the
//! format independent of the VM's own start-up, which is worth far more than
//! the pass costs.
//!
//! # Layout
//!
//! ```text
//! "WRENC\0"        magic
//! u16              format version
//! [u8; 32]         SHA-256 of the source this was compiled from
//! u32 + entries    method signatures, in the order they were interned
//! u32 + entries    module variable names, likewise
//! function         the module body, nested functions inline
//! ```
//!
//! Every length is a little-endian `u32` unless stated, and every string is a
//! `u32` length followed by its bytes -- Wren strings are bytes, so they are
//! written as bytes rather than as text.

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::bytecode::{Chunk, Op};
use crate::handle::ObjectId;
use crate::object::{ObjClosure, ObjFn, ObjString, Object, ObjectType};
use crate::value::Value;
use crate::vm::Vm;

/// `WRENC\0`.
const MAGIC: [u8; 6] = *b"WRENC\0";

/// Bumped whenever the layout changes in a way a reader cannot detect.
///
/// A file from a different version is refused rather than guessed at: the
/// failure of loading the wrong bytecode is an interpreter running nonsense,
/// which is far harder to diagnose than a message at load time.
pub const VERSION: u16 = 3;

/// What went wrong reading a `.wrenc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// Not a `.wrenc` at all.
    NotBytecode,
    /// A `.wrenc` from a different version of this format.
    WrongVersion { found: u16, expected: u16 },
    /// The file ended in the middle of something.
    Truncated,
    /// A tag the reader does not know, which means a file it cannot trust.
    Malformed(&'static str),
}

impl LoadError {
    pub fn message(&self) -> String {
        match self {
            LoadError::NotBytecode => "Not a .wrenc file.".to_string(),
            LoadError::WrongVersion { found, expected } => {
                alloc::format!("Bytecode is version {found}, this build reads version {expected}.")
            }
            LoadError::Truncated => "Bytecode ends unexpectedly.".to_string(),
            LoadError::Malformed(what) => alloc::format!("Bytecode is malformed: {what}."),
        }
    }
}

/// Which table a two-byte operand indexes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    /// An interned method signature.
    Symbol,
    /// A module variable.
    Variable,
}

/// Find every operand that indexes a table the VM owns.
///
/// **Decoding is the only way.** These operands are two bytes in the middle of
/// variable-length instructions, so a scan would find false matches in jump
/// offsets and constant indices. Walking the stream is also the check that it
/// is well formed: a stream that does not decode is one that would have jumped
/// into the middle of an instruction at run time.
fn table_operands(code: &[u16]) -> Result<Vec<(Table, usize)>, LoadError> {
    let mut found = Vec::new();
    let mut at = 0;

    // **One length table, in `Chunk::instruction_units`.** This used to carry
    // its own copy of how long each instruction is, which is a second thing to
    // update whenever an opcode is added -- and the fused pairs were added
    // without it, so the walk misaligned and every file failed to load.
    while at < code.len() {
        let Some(op) = Op::from_byte(Chunk::opcode_of(code[at])) else {
            return Err(LoadError::Malformed("unknown opcode"));
        };
        match op {
            // Every one of these carries its table index as a whole unit,
            // the one after the opcode's. `Call` puts its arity inline, so its
            // symbol is in the same place as the others'.
            Op::Call | Op::Super | Op::MethodInstance | Op::MethodStatic => {
                found.push((Table::Symbol, at + 1))
            }
            Op::LoadModuleVar | Op::StoreModuleVar => found.push((Table::Variable, at + 1)),
            _ => {}
        }
        let Some(len) = Chunk::instruction_units(code, at) else {
            return Err(LoadError::Truncated);
        };
        at += len;
    }

    if at != code.len() {
        return Err(LoadError::Malformed("an instruction runs past the end"));
    }
    Ok(found)
}

fn read_index(code: &[u16], at: usize) -> usize {
    code[at] as usize
}

fn write_index(code: &mut [u16], at: usize, value: usize) {
    code[at] = value as u16;
}

/// The names a chunk actually uses, gathered in first-use order.
///
/// **Only what is used travels.** Writing the VM's whole symbol table put four
/// hundred core-library signatures into every file and made the bytecode for a
/// twelve-line program larger than the program -- 2,953 bytes of bytecode for
/// 368 bytes of source, nearly all of it names nothing referenced.
#[derive(Default)]
struct Names {
    symbols: Vec<usize>,
    variables: Vec<usize>,
}

impl Names {
    fn gather(&mut self, vm: &Vm, chunk: &Chunk) -> Result<(), LoadError> {
        for (table, at) in table_operands(&chunk.code)? {
            let index = read_index(&chunk.code, at);
            let into = match table {
                Table::Symbol => &mut self.symbols,
                Table::Variable => &mut self.variables,
            };
            if !into.contains(&index) {
                into.push(index);
            }
        }

        // Nested functions use the same tables.
        for constant in &chunk.constants {
            if let Some(function) = constant.as_object().and_then(|id| vm.heap.function(id)) {
                let nested = function.chunk.clone();
                self.gather(vm, &nested)?;
            }
        }
        Ok(())
    }

    fn dense(&self, table: Table, index: usize) -> usize {
        let list = match table {
            Table::Symbol => &self.symbols,
            Table::Variable => &self.variables,
        };
        list.iter().position(|entry| *entry == index).unwrap_or(0)
    }
}

// --- writing ----------------------------------------------------------------

/// What to leave out of a written file.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Lines {
    /// Keep the line table, so a runtime error can say where it happened.
    #[default]
    Keep,
    /// Leave it out.
    ///
    /// **For a build that ships.** The table costs flash, costs the RAM it is
    /// read into, and is the one part of a `.wrenc` that maps the bytecode
    /// back to the source someone wrote -- which is worth something to whoever
    /// is reading the file and nothing to the program. What it buys back is an
    /// error that reports line 0.
    Strip,
}

/// Serialise a compiled chunk, stamped with the digest of its source.
///
/// Takes the VM because the symbol and variable names live there, not in the
/// chunk: what the chunk holds are indices into the VM's tables.
pub fn write(vm: &Vm, chunk: &Chunk, source: &[u8]) -> Result<Vec<u8>, LoadError> {
    write_with(vm, chunk, source, Lines::Keep)
}

/// As [`write`], choosing whether to keep the line table.
pub fn write_with(
    vm: &Vm,
    chunk: &Chunk,
    source: &[u8],
    lines: Lines,
) -> Result<Vec<u8>, LoadError> {
    let mut names = Names::default();
    names.gather(vm, chunk)?;

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&crate::sha256::digest(source));

    write_u32(&mut out, names.symbols.len() as u32);
    for index in &names.symbols {
        write_bytes(
            &mut out,
            vm.method_names.name(*index).unwrap_or("").as_bytes(),
        );
    }
    write_u32(&mut out, names.variables.len() as u32);
    for index in &names.variables {
        write_bytes(
            &mut out,
            vm.modules[0].names.name(*index).unwrap_or("").as_bytes(),
        );
    }

    let context = Writing {
        vm,
        names: &names,
        lines,
    };
    write_function(&context, &mut out, chunk, 0, 0, "(module)")?;
    Ok(out)
}

/// What every part of the writer needs: where names come from, what they are
/// renumbered to, and whether the line table is going in.
///
/// One struct rather than three arguments threaded through five functions --
/// `write_function` had grown to eight parameters, which is the point at which
/// adding the next one is somebody else's problem.
struct Writing<'a> {
    vm: &'a Vm,
    names: &'a Names,
    lines: Lines,
}

fn write_function(
    context: &Writing,
    out: &mut Vec<u8>,
    chunk: &Chunk,
    arity: usize,
    upvalues: usize,
    name: &str,
) -> Result<(), LoadError> {
    let Writing { names, lines, .. } = *context;
    out.push(arity as u8);
    out.push(upvalues as u8);
    write_bytes(out, name.as_bytes());

    // The code is emitted with its table operands renumbered to the dense
    // table this file carries.
    let mut code = chunk.code.clone();
    for (table, at) in table_operands(&code)? {
        let dense = names.dense(table, read_index(&code, at));
        write_index(&mut code, at, dense);
    }
    // **Units, little-endian.** The format carries what the chunk holds, so
    // a reader neither expands nor repacks.
    write_u32(out, code.len() as u32);
    for unit in &code {
        out.extend_from_slice(&unit.to_le_bytes());
    }

    // **The line table travels run-length encoded**, in the same shape the
    // chunk holds it: one entry per line, and the run lengths are the gaps
    // between entries. `Lines::Strip` writes none of it, for a build that
    // would rather not carry a map from its bytecode back to its source.
    let mut runs: Vec<(u16, u16)> = Vec::new();
    for (index, (start, line)) in chunk.lines.iter().enumerate() {
        if lines == Lines::Strip {
            break;
        }
        let end = match chunk.lines.get(index + 1) {
            Some((next, _)) => *next as usize,
            None => chunk.code.len(),
        };
        let mut remaining = end.saturating_sub(*start as usize);
        while remaining > 0 {
            let run = remaining.min(u16::MAX as usize);
            runs.push((run as u16, *line));
            remaining -= run;
        }
    }
    write_u32(out, runs.len() as u32);
    for (count, line) in runs {
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&line.to_le_bytes());
    }

    write_u32(out, chunk.constants.len() as u32);
    for constant in &chunk.constants {
        write_constant(context, out, *constant)?;
    }
    Ok(())
}

fn write_constant(context: &Writing, out: &mut Vec<u8>, value: Value) -> Result<(), LoadError> {
    let Writing { vm, .. } = *context;
    if value.is_null() {
        out.push(0);
        return Ok(());
    }
    if value.is_false() {
        out.push(1);
        return Ok(());
    }
    if value.is_true() {
        out.push(2);
        return Ok(());
    }
    if let Some(number) = value.as_num() {
        out.push(3);
        // **Always written as a double, whatever this build's `Num` is.** A
        // `.wrenc` is produced on a workstation and read on a part, and the
        // two need not agree about the width -- so the wider of the pair is
        // what goes on disk and a narrow loader rounds on the way in.
        #[allow(clippy::unnecessary_cast)]
        let wide = number as f64;
        out.extend_from_slice(&wide.to_bits().to_le_bytes());
        return Ok(());
    }
    let Some(id) = value.as_object() else {
        // Nothing else reaches a constant table.
        out.push(0);
        return Ok(());
    };
    // Each arm copies what it needs out of the heap before recursing, because
    // `write_constant` takes the VM again and a borrow could not outlive that.
    match vm.heap.type_of(id) {
        Some(ObjectType::String) => {
            out.push(4);
            let Some(bytes) = vm.heap.string(id).map(|text| text.bytes.clone()) else {
                return Ok(());
            };
            write_bytes(out, &bytes);
        }
        Some(ObjectType::Fn) => {
            out.push(5);
            let Some(function) = vm.heap.function(id) else {
                return Ok(());
            };
            let chunk = function.chunk.clone();
            let arity = function.arity;
            let upvalues = function.num_upvalues;
            let name = function.name.clone();
            write_function(context, out, &chunk, arity, upvalues, &name)?;
        }
        // **Class attributes are built at compile time**, so a whole object
        // graph -- maps of lists, wrapped in a `ClassAttributes` -- can be a
        // constant. It is the only such case, because it is the only thing the
        // compiler constructs rather than emits code to construct. Writing it
        // as null (the old fallback for "something else") lost every
        // attribute in a round trip, which showed up as five tests passing
        // from source and not from bytecode.
        Some(ObjectType::List) => {
            out.push(6);
            let Some(elements) = vm.heap.list(id).map(|list| list.elements.clone()) else {
                return Ok(());
            };
            write_u32(out, elements.len() as u32);
            for element in elements {
                write_constant(context, out, element)?;
            }
        }
        Some(ObjectType::Map) => {
            out.push(7);
            let Some(entries) = vm.heap.map(id).map(|map| {
                map.live()
                    .copied()
                    .collect::<Vec<crate::object::MapEntry>>()
            }) else {
                return Ok(());
            };
            write_u32(out, entries.len() as u32);
            for entry in entries {
                write_constant(context, out, entry.key)?;
                write_constant(context, out, entry.value)?;
            }
        }
        Some(ObjectType::Instance) => {
            out.push(8);
            let fields = vm.heap.instance_fields(id).to_vec();
            write_u32(out, fields.len() as u32);
            for field in fields {
                write_constant(context, out, field)?;
            }
        }
        // Nothing else reaches a constant table.
        _ => out.push(0),
    }
    Ok(())
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

// --- reading ----------------------------------------------------------------

/// A reader over a byte slice that refuses to run off the end.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], LoadError> {
        if self.at + count > self.bytes.len() {
            return Err(LoadError::Truncated);
        }
        let slice = &self.bytes[self.at..self.at + count];
        self.at += count;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, LoadError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, LoadError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, LoadError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, LoadError> {
        let bytes = self.take(8)?;
        let mut word = [0u8; 8];
        word.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(word))
    }

    fn blob(&mut self) -> Result<&'a [u8], LoadError> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    /// A length-prefixed run of instruction units, little-endian.
    ///
    /// The count is in units and each is two bytes, which is what the chunk
    /// holds -- so nothing is expanded or repacked on the way in.
    fn units(&mut self) -> Result<Vec<u16>, LoadError> {
        let count = self.u32()? as usize;
        let bytes = self.take(count * 2)?;
        Ok(bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect())
    }
}

/// What a loaded file was compiled from, so a caller can check it is current.
pub struct Loaded {
    /// The module body, ready to run.
    pub closure: ObjectId,
    /// SHA-256 of the source it was compiled from.
    pub source_digest: [u8; 32],
}

/// Read a `.wrenc` into `vm`, returning a closure ready to run.
pub fn load(vm: &mut Vm, bytes: &[u8]) -> Result<Loaded, LoadError> {
    let mut reader = Reader { bytes, at: 0 };

    if reader.take(MAGIC.len())? != MAGIC {
        return Err(LoadError::NotBytecode);
    }
    let version = reader.u16()?;
    if version != VERSION {
        return Err(LoadError::WrongVersion {
            found: version,
            expected: VERSION,
        });
    }

    let mut source_digest = [0u8; 32];
    source_digest.copy_from_slice(reader.take(32)?);

    // **The remapping tables.** Each name is interned into *this* VM, and the
    // index it lands at is what the code will be rewritten to use.
    let symbol_count = reader.u32()? as usize;
    let mut symbols = Vec::with_capacity(symbol_count);
    for _ in 0..symbol_count {
        let name = core::str::from_utf8(reader.blob()?)
            .map_err(|_| LoadError::Malformed("a method name is not utf-8"))?;
        symbols.push(vm.method_names.ensure(name) as u16);
    }

    let variable_count = reader.u32()? as usize;
    let mut variables = Vec::with_capacity(variable_count);
    for _ in 0..variable_count {
        let name = core::str::from_utf8(reader.blob()?)
            .map_err(|_| LoadError::Malformed("a variable name is not utf-8"))?;
        // A name the loading VM does not have is defined as null -- which is
        // what the compiler does for a forward reference, and what makes a
        // module compiled elsewhere land in the same shape here.
        let index = match vm.modules[0].names.find(name) {
            Some(index) => index,
            None => vm.modules[0].define(name, Value::NULL),
        };
        variables.push(index as u16);
    }

    let function = read_function(vm, &mut reader, &symbols, &variables)?;
    let closure = vm.heap.allocate(Object::Closure(ObjClosure {
        function,
        upvalues: Vec::new(),
    }));

    Ok(Loaded {
        closure,
        source_digest,
    })
}

fn read_function(
    vm: &mut Vm,
    reader: &mut Reader<'_>,
    symbols: &[u16],
    variables: &[u16],
) -> Result<ObjectId, LoadError> {
    let arity = reader.byte()? as usize;
    let upvalues = reader.byte()? as usize;
    let name = String::from_utf8_lossy(reader.blob()?).into_owned();

    let mut code = reader.units()?;

    // Straight into the compact form: one entry per line rather than per byte,
    // which is what the chunk holds. Expanding it here and compressing again
    // was a transient allocation the size of the code.
    let run_count = reader.u32()? as usize;
    let mut lines: Vec<(u32, u16)> = Vec::new();
    let mut offset = 0u32;
    for _ in 0..run_count {
        let count = reader.u16()?;
        let line = reader.u16()?;
        match lines.last() {
            Some((_, last)) if *last == line => {}
            _ => lines.push((offset, line)),
        }
        offset += u32::from(count);
    }

    let constant_count = reader.u32()? as usize;
    let mut constants = Vec::with_capacity(constant_count);
    for _ in 0..constant_count {
        constants.push(read_constant(vm, reader, symbols, variables)?);
    }

    remap(&mut code, symbols, variables)?;

    let function = vm.heap.allocate(Object::Fn(Box::new(ObjFn {
        chunk: Rc::new(Chunk::from_parts(code, constants, lines)),
        arity,
        num_upvalues: upvalues,
        name,
        field_offset: 0,
        super_class: None,
        owner_class: None,
        module: 0,
    })));
    Ok(function)
}

fn read_constant(
    vm: &mut Vm,
    reader: &mut Reader<'_>,
    symbols: &[u16],
    variables: &[u16],
) -> Result<Value, LoadError> {
    match reader.byte()? {
        0 => Ok(Value::NULL),
        1 => Ok(Value::FALSE),
        2 => Ok(Value::TRUE),
        3 => Ok(Value::num(
            f64::from_bits(reader.u64()?) as crate::value::Num
        )),
        4 => {
            let bytes = reader.blob()?.to_vec();
            Ok(Value::object(
                vm.heap.allocate(Object::String(ObjString::new(bytes))),
            ))
        }
        5 => Ok(Value::object(read_function(
            vm, reader, symbols, variables,
        )?)),
        6 => {
            let count = reader.u32()? as usize;
            let mut elements = Vec::with_capacity(count);
            for _ in 0..count {
                elements.push(read_constant(vm, reader, symbols, variables)?);
            }
            Ok(Value::object(vm.heap.allocate(Object::List(
                crate::object::ObjList { elements },
            ))))
        }
        7 => {
            let count = reader.u32()? as usize;
            // Rebuilt through the map's own insertion, so the table is hashed
            // and sized the way a map built at run time would be -- a map is
            // an open-addressed table, and its slots are not a thing to
            // serialise.
            let map = crate::core::new_map(vm);
            for _ in 0..count {
                let key = read_constant(vm, reader, symbols, variables)?;
                let value = read_constant(vm, reader, symbols, variables)?;
                crate::core::map_insert(vm, map, key, value);
            }
            Ok(map)
        }
        8 => {
            let count = reader.u32()? as usize;
            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                fields.push(read_constant(vm, reader, symbols, variables)?);
            }
            // The only instance that can be a constant is a `ClassAttributes`.
            let class = vm.class_attributes_class;
            Ok(Value::object(vm.heap.new_instance(class, &fields)))
        }
        _ => Err(LoadError::Malformed("unknown constant tag")),
    }
}

/// Rewrite the symbol and variable operands to this VM's indices.
///
/// **Walking the code is the only way to find them.** The operands are two
/// bytes in the middle of variable-length instructions, so the stream has to
/// be decoded rather than scanned -- which is also the check that the code is
/// well-formed, since a stream that does not decode is one that would have
/// jumped into the middle of an instruction at run time.
fn remap(code: &mut [u16], symbols: &[u16], variables: &[u16]) -> Result<(), LoadError> {
    for (table, at) in table_operands(code)? {
        let list = match table {
            Table::Symbol => symbols,
            Table::Variable => variables,
        };
        let dense = read_index(code, at);
        let Some(actual) = list.get(dense) else {
            return Err(LoadError::Malformed("an index is outside its table"));
        };
        write_index(code, at, *actual as usize);
    }
    Ok(())
}
