//! Threaded dispatch: one function per opcode, chained by guaranteed tail
//! calls. U8 in `doc/wren-rs/profiling.md`.
//!
//! **What upstream does, and why stable Rust cannot.** Upstream's interpreter
//! ends every instruction with `goto *dispatchTable[*ip++]` -- a computed
//! `goto`, so each instruction jumps straight to the next one's code and the
//! loop has no single dispatch point every instruction funnels through. Rust has
//! no computed `goto`. What it has, on nightly, is `become`: a call that is
//! *guaranteed* to replace the caller's stack frame rather than add one. With it
//! each opcode is its own function, and each ends by `become`-ing the next
//! opcode's function -- the same shape, and on RISC-V the same instructions: a
//! load from the handler table and a `jr`.
//!
//! # The two rules this is built to
//!
//! A first version ported every arm as it stood and measured **14% more work**
//! than the `match` loop. The reason is in how RISC-V calls work: a function
//! that calls another and keeps values past the call must save them in
//! callee-saved registers, and restore them before it returns -- or before it
//! `become`s the next handler. The `match` loop is one function and saves them
//! once per invocation; a handler saves them *every time it runs*. So:
//!
//! 1. **Few arguments.** Every handler receives four: the machine, the
//!    instruction pointer, the opcode unit, and the handler table. Everything
//!    else about the running frame -- its stack base, its constant pool, its
//!    chunk, its module, where its fields start -- is in [`State`], a field of
//!    the `Vm`, read by the handlers that want it. An argument costs a save and
//!    a restore in every handler that calls something; a field costs a load in
//!    the handlers that use it.
//! 2. **A hot handler calls nothing that returns into it.** Anything rare --
//!    an error, a growing push, closing upvalues, a slow call -- leaves by
//!    `become` into a cold handler of the same signature, so the hot handler
//!    keeps no values alive across a call and needs no saved registers at all.
//!    The one exception is arithmetic on doubles, which is a call into the
//!    ROM's soft-float routines: there is no way round that one.
//!
//! LLVM's `preserve_none` convention -- how CPython's tail-call interpreter
//! escapes the same cost -- is accepted by the compiler for this target and
//! changes nothing: the RISC-V backend treats it as the C convention.
//!
//! # The contract with `run_frames`
//!
//! [`run`] replaces the `match` loop's body and keeps its contract exactly:
//! the same frames parked and resumed the same way, the same errors delivered
//! to a catching fiber or returned, the same collection points. Where the loop
//! has a quirk, this has it too -- the aim is a faster version of the same
//! machine, measured against the same 829 programs, not a different one.
//!
//! **Nested invocations.** A primitive can re-enter the interpreter --
//! `Fn.call`, every `Sequence` method taking a block, `toString` through
//! `stringify` -- and the nested [`run`] overwrites [`State`]. So [`run`] saves
//! it on entry and puts it back on exit, and the outer chain resumes as though
//! nothing happened.

use super::{
    chunk_constants, chunk_of, code_units, ChunkRef, Frame, RuntimeError, Switch, Vm,
    INTRINSIC_ITERATE, MAX_FRAMES, NUM_ADD, NUM_DIV, NUM_GE, NUM_GT, NUM_LE, NUM_LT,
    NUM_MOD, NUM_MUL, NUM_OP_NONE, NUM_SUB, NURSERY, NURSERY_OBJECTS,
};
use crate::bytecode::{code, Chunk};
use crate::object::{Method, ObjClosure, Object};
use crate::value::Value;

extern crate alloc;
use alloc::vec::Vec;

// Neither instrument has been taught this loop: `profile` counts per
// instruction inside `run_frames`' body, and `verify-slots` checks the stack
// bound there. Both still work with the feature off, which is how the bound
// this module relies on -- `push_reserved` -- is proved.
#[cfg(feature = "profile")]
compile_error!("the `profile` feature counts inside the `match` loop; build it without `tail-calls`");
#[cfg(feature = "verify-slots")]
compile_error!("`verify-slots` checks inside the `match` loop; build it without `tail-calls`");

/// How a chain of handlers ends.
///
/// **One byte, returned in a register.** `run_frames` returns
/// `Result<Value, RuntimeError>`, which is wider than two words, so on this
/// target it would come back through a hidden pointer in `a0` -- one argument
/// register fewer for every handler. The value goes into [`State`] and the
/// error into `Vm::threaded_error` instead, and this says which.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Exit {
    /// `State::result` holds the value this invocation returns.
    Returned,
    /// `Vm::threaded_error` holds the error this invocation fails with.
    Failed,
}

/// What a handler needs about the running frame, kept in the `Vm` rather than
/// in registers -- see rule 1 in the module notes.
///
/// **Written by a call, a return, a switch or an error**, which is when the
/// running frame changes; read by the handlers that need each field. The
/// first five are the `match` loop's locals of the same names.
#[derive(Clone, Copy)]
pub(super) struct State {
    /// The stack slot of the running frame's receiver. Local *n* is `base + n`.
    base: usize,
    /// The running chunk's constant pool.
    ///
    /// `&'static` is the promise [`chunk_constants`] makes: the slice lives as
    /// long as the frame whose chunk it is.
    constants: &'static [Value],
    /// The running frame's code. Its `Frame::chunk` is `None` while it runs,
    /// exactly as in `run_frames`.
    chunk: Option<ChunkRef>,
    /// Which module's variables `LoadModuleVar` reads.
    module: usize,
    /// Where the running method's own fields start in its receiver.
    field_offset: usize,
    /// The frame depth at which this invocation returns to its Rust caller.
    floor: usize,
    /// A return's value, on its way out of the chain or into the slow return.
    result: Value,
}

impl State {
    pub(super) const fn new() -> State {
        State {
            base: 0,
            constants: &[],
            chunk: None,
            module: 0,
            field_offset: 0,
            floor: 0,
            result: Value::NULL,
        }
    }
}

/// Every handler's signature: the machine, the unit after the opcode, the
/// opcode unit itself (for its inline operand), and the handler table.
///
/// **The table is an argument** so that the jump to the next handler does not
/// rebuild its address -- a `lui` and an `addi` -- on every instruction.
type Handler = fn(&mut Vm, *const u16, u32, &'static Table) -> Exit;

/// The handlers, indexed by an opcode's six bits.
///
/// A struct rather than a bare array only so that [`Handler`] can name it: a
/// type alias cannot refer to itself, a struct reached through a reference can.
pub(super) struct Table([Handler; 64]);

/// The chunk that is running, which every path that needs it has.
///
/// **`expect` rather than a default**, because `run` sets it before the first
/// handler runs and nothing clears it -- a `None` here is a bug in this module,
/// and a panic says so where a made-up chunk would not.
#[inline(always)]
fn running(vm: &Vm) -> ChunkRef {
    vm.threaded.chunk.expect("a running chunk")
}

/// The unit offset of `cursor` in the running chunk, for everything that
/// speaks in offsets: `Frame::ip`, `line_at`, `deliver_error`, `collect_point`.
#[inline(always)]
#[allow(unsafe_code)]
fn offset(vm: &Vm, cursor: *const u16) -> usize {
    let start = code_units(running(vm)).as_ptr();
    // SAFETY: every cursor a handler holds points into the running chunk's
    // code, and `start` is that code's first unit.
    (unsafe { cursor.offset_from(start) }) as usize
}

/// The source line of the instruction whose operands start at `ip` -- the
/// opcode is the unit before, which is the `at` the `match` loop keeps.
#[inline(always)]
#[allow(unsafe_code)]
fn line_of(vm: &Vm, ip: *const u16) -> u16 {
    // SAFETY: a handler's `ip` is one past its opcode, never the first unit.
    let at = unsafe { ip.sub(1) };
    chunk_of(running(vm)).line_at(offset(vm, at))
}

/// Start running the frame on top, from wherever it was parked, and say where.
///
/// **The one place a frame becomes the running one** -- after a call, a
/// return, a fiber switch or a delivered error. It fills [`State`] from the
/// frame; nothing in it calls out, so a handler that uses it stays a leaf.
#[inline(always)]
#[allow(unsafe_code)]
fn enter(vm: &mut Vm, chunk: ChunkRef) -> *const u16 {
    let frame = vm.frames.last().expect("a frame to run");
    let (at, base, module, field_offset) = (frame.ip, frame.base, frame.module, frame.field_offset);
    vm.threaded.chunk = Some(chunk);
    vm.threaded.base = base;
    vm.threaded.constants = chunk_constants(chunk);
    vm.threaded.module = module;
    vm.threaded.field_offset = field_offset as usize;
    // SAFETY: a parked frame's offset is into its own chunk.
    unsafe { code_units(chunk).as_ptr().add(at) }
}

/// Resume the frame on top after a switch, taking its chunk back first.
fn resume(vm: &mut Vm) -> Result<*const u16, RuntimeError> {
    let chunk = vm.resume_chunk()?;
    Ok(enter(vm, chunk))
}

/// Fetch the next instruction and become its handler.
///
/// **`become`, not a call.** The current handler's frame is replaced, so a
/// chain of a million instructions uses one frame's worth of stack. The
/// compiler refuses `become` unless the callee's signature is exactly the
/// caller's, and checks that in the function it is written in -- so only a
/// handler can use this.
macro_rules! dispatch {
    ($vm:expr, $ip:expr, $table:expr) => {{
        let ip: *const u16 = $ip;
        // SAFETY: as the `match` loop's fetch -- `ip` moves only by whole
        // instructions, every chunk ends in one that leaves, and every jump is
        // patched against its own chunk; loaded bytecode is walked whole first.
        #[allow(unsafe_code)]
        let unit = unsafe { *ip } as u32;
        // **No range check and no `unsafe` for it.** The table has 64 entries
        // and the mask keeps the index below 64, so the compiler drops the
        // bounds check; the 24 entries past the last opcode are `bad_opcode`.
        let table: &'static Table = $table;
        let handler = table.0[(unit & 0x3f) as usize];
        #[allow(unsafe_code)]
        let next = unsafe { ip.add(1) };
        become handler($vm, next, unit, table)
    }};
}

/// Leave the chain with an error, which `run` returns. For cold handlers:
/// building an error is a call, which a hot handler must not make.
macro_rules! fail {
    ($vm:expr, $error:expr) => {{
        $vm.threaded_error = Some($error);
        return Exit::Failed;
    }};
}

/// `?` for a cold handler: an `Err` ends the chain with that error.
macro_rules! attempt {
    ($vm:expr, $result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => fail!($vm, error),
        }
    };
}

/// Where the chain goes next, from a helper that is not a handler and so
/// cannot `become`: an instruction to run, or out. The handler that called the
/// helper follows it with [`go!`].
///
/// A helper that *called* the next handler instead would leave its own frame
/// under the rest of the chain, and a program that switches fibers in a loop
/// would grow the Rust stack by one frame per switch.
type Next = Result<*const u16, Exit>;

/// Follow a [`Next`] from inside a handler.
macro_rules! go {
    ($vm:expr, $next:expr, $table:expr) => {
        match $next {
            Ok(ip) => dispatch!($vm, ip, $table),
            Err(exit) => exit,
        }
    };
}

/// `?` for a helper returning [`Next`]: an `Err` ends the chain.
macro_rules! next_or_fail {
    ($vm:expr, $result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => {
                $vm.threaded_error = Some(error);
                return Err(Exit::Failed);
            }
        }
    };
}

/// Collect after an instruction that allocated; see `collect_if_due!` in
/// `run_frames`, whose argument this repeats. Only cold handlers use it --
/// every allocating instruction is rare, and the collection itself is a call.
macro_rules! collect_if_due {
    ($vm:expr, $ip:expr) => {{
        if (NURSERY && $vm.heap.young() >= NURSERY_OBJECTS) || $vm.heap.collection_due() {
            let at = offset($vm, $ip);
            $vm.collect_point(at);
        }
    }};
}

/// Deliver an error to a catching fiber and carry on there, or end the chain.
///
/// The `match` loop's `deliver_error(..)?` and the resume after it. `at` is
/// the offset parked into the failing frame, as the loop passes it.
fn delivered(vm: &mut Vm, error: RuntimeError, at: usize) -> Next {
    let chunk = running(vm);
    match vm.deliver_error(error, at, chunk) {
        Ok(Some((next_chunk, _, _))) => Ok(enter(vm, next_chunk)),
        Ok(None) => unreachable!("deliver_error returns or switches"),
        Err(error) => {
            vm.threaded_error = Some(error);
            Err(Exit::Failed)
        }
    }
}

/// Run the frame on top of `vm.frames` until this invocation returns.
///
/// The `tail-calls` body of `run_frames`; see the module notes.
pub(super) fn run(vm: &mut Vm, chunk: ChunkRef, floor: usize) -> Result<Value, RuntimeError> {
    // **Saved, because a primitive can call back into the interpreter** and the
    // nested invocation writes the same fields. See the module notes.
    let saved = vm.threaded;
    vm.threaded.floor = floor;
    let ip = enter(vm, chunk);

    // The first handler is *called*: `become` needs the caller's signature to
    // be the callee's, and this function's is not a handler's. Every handler
    // after it is reached by `become`, and whichever one ends the chain returns
    // straight here.
    #[allow(unsafe_code)]
    let unit = unsafe { *ip } as u32;
    let handler = TABLE.0[(unit & 0x3f) as usize];
    #[allow(unsafe_code)]
    let exit = handler(vm, unsafe { ip.add(1) }, unit, &TABLE);

    let result = vm.threaded.result;
    vm.threaded = saved;
    match exit {
        Exit::Returned => Ok(result),
        Exit::Failed => Err(vm.threaded_error.take().expect("a failing handler leaves its error")),
    }
}

/// The handler table.
///
/// Built in a `const` block so each entry is named by the opcode constant it
/// answers -- the table cannot get out of step with `bytecode::code` by
/// position, which a literal list of 64 would invite.
static TABLE: Table = Table({
    let mut table: [Handler; 64] = [bad_opcode as Handler; 64];
    table[code::CONSTANT as usize] = constant;
    table[code::NULL as usize] = null;
    table[code::FALSE as usize] = r#false;
    table[code::TRUE as usize] = r#true;
    table[code::LOAD_LOCAL as usize] = load_local;
    table[code::LOAD_LOCAL_CONSTANT as usize] = load_local_constant;
    table[code::LOAD_LOCAL_PAIR as usize] = load_local_pair;
    table[code::STORE_FIELD_THIS_POP as usize] = store_field_this_pop;
    table[code::STORE_LOCAL as usize] = store_local;
    table[code::LOAD_MODULE_VAR as usize] = load_module_var;
    table[code::STORE_MODULE_VAR as usize] = store_module_var;
    table[code::POP as usize] = pop;
    table[code::LOAD_UPVALUE as usize] = load_upvalue;
    table[code::STORE_UPVALUE as usize] = store_upvalue;
    table[code::CLOSE_UPVALUE as usize] = close_upvalue;
    table[code::CLOSURE as usize] = closure;
    table[code::CLASS as usize] = class;
    table[code::METHOD_INSTANCE as usize] = method_instance;
    table[code::METHOD_STATIC as usize] = method_static;
    table[code::LOAD_FIELD_THIS as usize] = load_field_this;
    table[code::STORE_FIELD_THIS as usize] = store_field_this;
    table[code::LOAD_FIELD as usize] = load_field;
    table[code::STORE_FIELD as usize] = store_field;
    table[code::CONSTRUCT as usize] = construct;
    table[code::CALL as usize] = call;
    table[code::SUPER as usize] = super_call;
    table[code::LOAD_STATIC_FIELD as usize] = load_static_field;
    table[code::STORE_STATIC_FIELD as usize] = store_static_field;
    table[code::SET_ATTRIBUTES as usize] = set_attributes;
    table[code::IMPORT_MODULE as usize] = import_module;
    table[code::IMPORT_VARIABLE as usize] = import_variable;
    table[code::RETURN as usize] = r#return;
    table[code::END as usize] = end;
    table[code::LOAD_LOCAL_RETURN as usize] = load_local_return;
    table[code::LOAD_FIELD_THIS_RETURN as usize] = load_field_this_return;
    table[code::JUMP as usize] = jump;
    table[code::LOOP as usize] = r#loop;
    table[code::JUMP_IF as usize] = jump_if;
    table[code::AND as usize] = and;
    table[code::OR as usize] = or;
    table
});

// ---------------------------------------------------------------------------
// Hot handlers: leaves, by rule 2. Each is the `match` arm of the same name in
// `run_frames`, whose comments explain the *why*; these explain what differs.
// ---------------------------------------------------------------------------

#[allow(unsafe_code)]
fn constant(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    // SAFETY: an operand unit of this instruction.
    let index = unsafe { *ip } as usize;
    let value = vm.threaded.constants[index];
    vm.push_reserved(value);
    dispatch!(vm, unsafe { ip.add(1) }, table)
}

fn null(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    vm.push_reserved(Value::NULL);
    dispatch!(vm, ip, table)
}

fn r#false(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    vm.push_reserved(Value::FALSE);
    dispatch!(vm, ip, table)
}

fn r#true(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    vm.push_reserved(Value::TRUE);
    dispatch!(vm, ip, table)
}

fn load_local(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let slot = Chunk::inline_operand(unit as u16) as usize;
    let value = vm.stack[vm.threaded.base + slot];
    vm.push_reserved(value);
    dispatch!(vm, ip, table)
}

#[allow(unsafe_code)]
fn load_local_constant(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let slot = Chunk::inline_operand(unit as u16) as usize;
    // SAFETY: three units, the constant index in the third.
    let index = unsafe { *ip.add(1) } as usize;
    let local = vm.stack[vm.threaded.base + slot];
    let constant = vm.threaded.constants[index];
    vm.push_reserved_pair(local, constant);
    dispatch!(vm, unsafe { ip.add(2) }, table)
}

#[allow(unsafe_code)]
fn load_local_pair(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let base = vm.threaded.base;
    let first = Chunk::inline_operand(unit as u16) as usize;
    // SAFETY: the second slot rides in the second unit.
    let second = Chunk::inline_operand(unsafe { *ip }) as usize;
    // The second slot can be the one the first push writes into; see the arm.
    let a = vm.stack[base + first];
    let top = vm.stack.len();
    let b = match base + second == top {
        true => a,
        false => vm.stack[base + second],
    };
    vm.push_reserved_pair(a, b);
    dispatch!(vm, unsafe { ip.add(1) }, table)
}

#[allow(unsafe_code)]
fn store_field_this_pop(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let index = Chunk::inline_operand(unit as u16) as usize;
    let value = vm.stack.pop().unwrap_or(Value::NULL);
    let receiver = vm.stack[vm.threaded.base];
    let offset = vm.threaded.field_offset;
    if !vm.set_field(receiver, offset, index, value) {
        become not_a_field(vm, ip, unit, table);
    }
    dispatch!(vm, unsafe { ip.add(1) }, table)
}

fn store_local(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let slot = Chunk::inline_operand(unit as u16) as usize;
    let top = *vm.stack.last().unwrap();
    let base = vm.threaded.base;
    vm.stack[base + slot] = top;
    dispatch!(vm, ip, table)
}

#[allow(unsafe_code)]
fn load_module_var(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let index = unsafe { *ip } as usize;
    let value = vm.modules[vm.threaded.module].values[index];
    vm.push_reserved(value);
    dispatch!(vm, unsafe { ip.add(1) }, table)
}

#[allow(unsafe_code)]
fn store_module_var(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let index = unsafe { *ip } as usize;
    let top = *vm.stack.last().unwrap();
    let module = vm.threaded.module;
    vm.modules[module].values[index] = top;
    dispatch!(vm, unsafe { ip.add(1) }, table)
}

fn pop(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    vm.stack.pop();
    dispatch!(vm, ip, table)
}

fn load_field_this(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let index = Chunk::inline_operand(unit as u16) as usize;
    let offset = vm.threaded.field_offset;
    let Some(value) = vm.field_of(vm.stack[vm.threaded.base], offset, index) else {
        become not_a_field(vm, ip, unit, table);
    };
    // Within the frame's reservation, as every push an instruction makes is.
    vm.push_reserved(value);
    dispatch!(vm, ip, table)
}

fn construct(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let base = vm.threaded.base;
    let class = vm.stack[base];
    let instance = attempt!(vm, vm.instantiate(class));
    vm.stack[base] = instance;
    collect_if_due!(vm, ip);
    dispatch!(vm, ip, table)
}

// Jumps.

#[allow(unsafe_code)]
fn jump(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let distance = unsafe { *ip } as usize;
    dispatch!(vm, unsafe { ip.add(1 + distance) }, table)
}

#[allow(unsafe_code)]
fn r#loop(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let distance = unsafe { *ip } as usize;
    dispatch!(vm, unsafe { ip.add(1).sub(distance) }, table)
}

#[allow(unsafe_code)]
fn jump_if(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let distance = unsafe { *ip } as usize;
    let ip = unsafe { ip.add(1) };
    let condition = vm.stack.pop().unwrap_or(Value::NULL);
    let ip = match condition.is_falsy() {
        true => unsafe { ip.add(distance) },
        false => ip,
    };
    dispatch!(vm, ip, table)
}

#[allow(unsafe_code)]
fn and(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let distance = unsafe { *ip } as usize;
    let mut ip = unsafe { ip.add(1) };
    if vm.stack.last().copied().unwrap_or(Value::NULL).is_falsy() {
        ip = unsafe { ip.add(distance) };
    } else {
        vm.stack.pop();
    }
    dispatch!(vm, ip, table)
}

#[allow(unsafe_code)]
fn or(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let distance = unsafe { *ip } as usize;
    let mut ip = unsafe { ip.add(1) };
    if vm.stack.last().copied().unwrap_or(Value::NULL).is_falsy() {
        vm.stack.pop();
    } else {
        ip = unsafe { ip.add(distance) };
    }
    dispatch!(vm, ip, table)
}

// ---------------------------------------------------------------------------
// Calls. `call` keeps only the arithmetic fast path, the one place a hot
// handler calls out (to soft-float); everything else leaves for
// `call_intrinsic`, `call_method` or `call_general`.
// ---------------------------------------------------------------------------

#[allow(unsafe_code)]
fn call(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let arity = Chunk::inline_operand(unit as u16) as usize;
    let symbol = unsafe { *ip } as usize;
    if arity <= 1 {
        let operation = match vm.num_ops.get(symbol) {
            Some(operation) => *operation,
            None => NUM_OP_NONE,
        };
        if operation != NUM_OP_NONE && operation <= NUM_GE {
            let top = vm.stack.len();
            let left = vm.stack[top - 2];
            let right = vm.stack[top - 1];
            if left.is_num() && right.is_num() {
                let (a, b) = (left.num_unchecked(), right.num_unchecked());
                #[allow(clippy::float_cmp)]
                let value = match operation {
                    NUM_DIV | NUM_MOD
                        if cfg!(feature = "nofp") && b == (0 as crate::value::Num) =>
                    {
                        None
                    }
                    NUM_ADD => Some(Value::num(a + b)),
                    NUM_SUB => Some(Value::num(a - b)),
                    NUM_MUL => Some(Value::num(a * b)),
                    NUM_DIV => Some(Value::num(a / b)),
                    NUM_MOD => Some(Value::num(a % b)),
                    NUM_LT => Some(Value::bool(a < b)),
                    NUM_GT => Some(Value::bool(a > b)),
                    NUM_LE => Some(Value::bool(a <= b)),
                    _ => Some(Value::bool(a >= b)),
                };
                if let Some(value) = value {
                    // The receiver's slot is free again, and the stack held
                    // two values a moment ago: room by the same argument as a
                    // return's result.
                    vm.stack.truncate(top - 2);
                    vm.push_reserved(value);
                    dispatch!(vm, unsafe { ip.add(1) }, table);
                }
            }
        } else if operation >= INTRINSIC_ITERATE {
            become call_intrinsic(vm, ip, unit, table);
        }
    }
    become call_method(vm, ip, unit, table)
}

/// A call whose symbol names an intrinsic: answered here if the receiver
/// allows, otherwise the ordinary call.
///
/// **Never inlined**, like every cold continuation: LLVM may inline the
/// target of a direct `become`, and inlining this into the hot handler that
/// leaves to it would bring its calls -- and the register-saving frame they
/// need -- back onto the hot path it was split off from.
#[inline(never)]
#[allow(unsafe_code)]
fn call_intrinsic(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let arity = Chunk::inline_operand(unit as u16) as usize;
    let symbol = unsafe { *ip } as usize;
    let operation = match vm.num_ops.get(symbol) {
        Some(operation) => *operation,
        None => NUM_OP_NONE,
    };
    let receiver_at = vm.stack.len() - arity - 1;
    if let Some(value) = vm.intrinsic(operation, receiver_at, symbol) {
        vm.stack.truncate(receiver_at);
        vm.push_reserved(value);
        dispatch!(vm, unsafe { ip.add(1) }, table);
    }
    become call_general(vm, ip, unit, table)
}

/// The common call, kept a leaf: an instance or a class receiving a method
/// written in Wren, with room already there for the callee's frame.
///
/// **Only those two receivers, and only closures.** Widening it to every
/// object's class and giving primitives a handler of their own was measured:
/// `list_build` gained 3% in `nofp` and `fib` lost 1% in `f64`, whose calls
/// never reach either -- the bigger handler cost the path it did not change.
///
/// **Everything else leaves unchanged for [`call_general`]** -- a primitive,
/// a number or a list as the receiver, a missing method, a wrong arity, a
/// stack or frame list that has to grow -- which does the whole call the long
/// way. Nothing here is changed before that decision, so handing over is
/// always safe: `call_general` sees exactly the state this handler saw.
///
/// Growing is what keeps this a leaf. `Vec::reserve` and `Vec::push` each hide
/// a call to the allocator for when there is no room, and a call that can
/// return puts a register-saving frame on every execution; testing for room
/// here and sending the rare case elsewhere keeps both out.
#[inline(never)]
#[allow(unsafe_code)]
fn call_method(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let arity = Chunk::inline_operand(unit as u16) as usize;
    let symbol = unsafe { *ip } as usize;
    let receiver_at = vm.stack.len() - arity - 1;

    // The receiver's class, for every object -- the handle carries the type.
    // Numbers, booleans and null go the long way.
    let class = match vm.stack[receiver_at].as_object() {
        Some(id) => match vm.heap.kind_of(id) {
            Some(crate::object::ObjectType::Instance) => vm.heap.instance(id).map(|instance| instance.class),
            Some(crate::object::ObjectType::Class) => vm
                .heap
                .class(id)
                .map(|class| class.metaclass.unwrap_or(vm.class_class)),
            _ => None,
        },
        None => None,
    };
    let Some(class) = class else {
        become call_general(vm, ip, unit, table);
    };
    let entry = match vm.heap.class(class) {
        Some(class) => class.method_entry(symbol),
        None => crate::object::NO_METHOD,
    };
    // A method written in Wren, which is what a leaf can do. A primitive runs
    // in a handler of its own, which can afford the frame a call into
    // arbitrary code needs; a missing method goes the long way.
    let Some(closure) = crate::object::entry_closure(entry) else {
        become call_general(vm, ip, unit, table);
    };
    let Some(function) = vm.heap.closure(closure).and_then(|closure| vm.heap.function(closure.function)) else {
        become call_general(vm, ip, unit, table);
    };
    let (chunk, max_slots, module, field_offset) = (
        ChunkRef::from(&*function.chunk),
        function.max_slots,
        function.module,
        function.field_offset as u16,
    );
    // Wrong arity, too deep, or no room: all the long way.
    if function.arity != arity
        || vm.frames.len() >= MAX_FRAMES
        || vm.frames.len() == vm.frames.capacity()
        || vm.stack.capacity() - vm.stack.len() < max_slots
    {
        become call_general(vm, ip, unit, table);
    }

    // The caller stops running here: park its `ip` and its chunk.
    let next = unsafe { ip.add(1) };
    let caller_ip = offset(vm, next);
    let caller_chunk = running(vm);
    if let Some(frame) = vm.frames.last_mut() {
        frame.ip = caller_ip;
        frame.chunk = Some(caller_chunk);
    }
    // Room was tested above, so this cannot reach the allocator.
    let pushed = vm.frames.push_within_capacity(Frame {
        closure,
        ip: 0,
        base: receiver_at,
        chunk: None,
        module,
        field_offset,
    });
    debug_assert!(pushed.is_ok(), "room for a frame was tested");
    vm.threaded.chunk = Some(chunk);
    vm.threaded.base = receiver_at;
    vm.threaded.constants = chunk_constants(chunk);
    vm.threaded.module = module;
    vm.threaded.field_offset = field_offset as usize;
    dispatch!(vm, code_units(chunk).as_ptr(), table)
}

/// What a primitive's result means for the chain: an error delivered, the
/// program halting, a fiber switch, or the value pushed and the next
/// instruction.
#[inline(always)]
fn primitive_outcome(vm: &mut Vm, outcome: Result<Value, RuntimeError>, receiver_at: usize, next: *const u16) -> Next {
    let value = match outcome {
        Ok(value) => value,
        Err(mut error) => {
            if error.line == 0 {
                error.line = line_of(vm, call_operands(next));
            }
            let at = offset(vm, next);
            return delivered(vm, error, at);
        }
    };
    vm.stack.truncate(receiver_at);
    if vm.halting {
        vm.halting = false;
        vm.threaded.result = Value::NULL;
        return Err(Exit::Returned);
    }
    if let Some(switch) = vm.pending_switch.take() {
        return switch_fiber(vm, switch, next);
    }
    vm.stack.push(value);
    collect_if_due!(vm, next);
    Ok(next)
}

/// A call the long way: any receiver, any method, errors delivered, room made.
#[inline(never)]
#[allow(unsafe_code)]
fn call_general(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let arity = Chunk::inline_operand(unit as u16) as usize;
    let symbol = unsafe { *ip } as usize;
    let next = unsafe { ip.add(1) };
    let receiver_at = vm.stack.len() - arity - 1;
    let receiver = vm.stack[receiver_at];
    let Some(class) = vm.class_of(receiver) else {
        fail!(vm, RuntimeError::new("Receiver has no class."));
    };
    go!(vm, invoke(vm, next, arity, symbol, class), table)
}

#[allow(unsafe_code)]
fn super_call(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let arity = Chunk::inline_operand(unit as u16) as usize;
    let symbol = unsafe { *ip } as usize;
    let next = unsafe { ip.add(1) };
    let class = vm
        .frames
        .last()
        .and_then(|frame| vm.function_of(frame.closure))
        .and_then(|function| function.super_class);
    let Some(class) = class else {
        fail!(vm, RuntimeError::new("Cannot use 'super' outside of a method."));
    };
    go!(vm, invoke(vm, next, arity, symbol, class), table)
}

/// The instruction a call's line is reported against: `next` is past the
/// symbol unit, and `line_of` wants the unit after the opcode.
#[inline(always)]
#[allow(unsafe_code)]
fn call_operands(next: *const u16) -> *const u16 {
    // SAFETY: `next` is two units into a call instruction.
    unsafe { next.sub(1) }
}

/// Find `symbol` on `class` and run it: a primitive in place, a closure by
/// entering a new frame. Returns where the chain goes next; see [`Next`].
#[inline(always)]
fn invoke(vm: &mut Vm, next: *const u16, arity: usize, symbol: usize, class: crate::handle::ObjectId) -> Next {
    let receiver_at = vm.stack.len() - arity - 1;
    let Some(method) = vm.find_method(class, symbol) else {
        let line = line_of(vm, call_operands(next));
        let error = vm.no_such_method(receiver_at, symbol, line);
        let at = offset(vm, next);
        return delivered(vm, error, at);
    };

    match method {
        Method::Primitive(function) => {
            let outcome = function(vm, receiver_at);
            primitive_outcome(vm, outcome, receiver_at, next)
        }
        Method::Closure(closure) => {
            let target = next_or_fail!(vm, vm.call_target(closure));
            if target.arity != arity {
                let line = line_of(vm, call_operands(next));
                vm.threaded_error = Some(Vm::wrong_arity(target.arity, arity, line));
                return Err(Exit::Failed);
            }
            if vm.frames.len() >= MAX_FRAMES {
                let line = line_of(vm, call_operands(next));
                vm.threaded_error = Some(Vm::stack_overflow(line));
                return Err(Exit::Failed);
            }
            // The caller stops running here: park its `ip` and its chunk,
            // offset first, while it still refers to the caller's code.
            let caller_ip = offset(vm, next);
            let caller_chunk = running(vm);
            if let Some(frame) = vm.frames.last_mut() {
                frame.ip = caller_ip;
                frame.chunk = Some(caller_chunk);
            }
            vm.stack.reserve(target.max_slots);
            vm.frames.push(Frame {
                closure,
                ip: 0,
                base: receiver_at,
                chunk: None,
                module: target.module,
                field_offset: target.field_offset,
            });
            vm.threaded.chunk = Some(target.chunk);
            vm.threaded.base = receiver_at;
            vm.threaded.constants = chunk_constants(target.chunk);
            vm.threaded.module = target.module;
            vm.threaded.field_offset = target.field_offset as usize;
            Ok(code_units(target.chunk).as_ptr())
        }
    }
}

/// A primitive asked to continue in another fiber: `perform_switch`, and for
/// `transferError` an error delivered there, then run whatever is on top.
#[inline(never)]
fn switch_fiber(vm: &mut Vm, switch: Switch, next: *const u16) -> Next {
    let failing = switch.as_error.then_some(switch.value);
    let at = offset(vm, next);
    let line = line_of(vm, call_operands(next));
    let chunk = running(vm);
    next_or_fail!(vm, vm.perform_switch(switch, at, chunk));
    if let Some(value) = failing {
        let message = vm.to_string(value);
        let error = RuntimeError { message, line };
        // The loop passes offset 0 and the pre-switch chunk here; so does this.
        return match vm.deliver_error(error, 0, chunk) {
            Ok(Some((next_chunk, _, _))) => Ok(enter(vm, next_chunk)),
            Ok(None) => unreachable!("deliver_error returns or switches"),
            Err(error) => {
                vm.threaded_error = Some(error);
                Err(Exit::Failed)
            }
        };
    }
    Ok(next_or_fail!(vm, resume(vm)))
}

// ---------------------------------------------------------------------------
// Returns. The common case -- no open upvalues, a caller in this invocation
// whose chunk was parked -- is a leaf; everything else is `return_slow`'s.
// ---------------------------------------------------------------------------

/// The shared end of the four return handlers.
///
/// A macro rather than a function because it ends in `become`, which only a
/// handler can do. `result` is the value; `ip` and `unit` are the handler's
/// own, handed on to the slow path so it can report and park as the arm does.
macro_rules! finish_return {
    ($vm:expr, $result:expr, $ip:expr, $unit:expr, $table:expr) => {{
        let result: Value = $result;
        let base = $vm.threaded.base;
        // **Two tests keep the common return a leaf.** Closing upvalues calls
        // out, and so does leaving at the floor; both are rare, and both go
        // to the slow path whole, before anything is changed.
        if !$vm.open_upvalues.is_empty() || $vm.frames.len() <= $vm.threaded.floor + 1 {
            $vm.threaded.result = result;
            become return_slow($vm, $ip, $unit, $table);
        }
        $vm.stack.truncate(base);
        $vm.frames.pop();
        // Room by the argument in the arm: the slot at `base` held the receiver.
        $vm.push_reserved(result);
        // Take the caller's chunk back. `None` means nobody parked it -- a
        // primitive that re-entered -- and deriving it is a heap walk.
        let chunk = match $vm.frames.last_mut() {
            Some(frame) => frame.chunk.take(),
            None => None,
        };
        let Some(chunk) = chunk else {
            become resume_derived($vm, $ip, $unit, $table);
        };
        let ip = enter($vm, chunk);
        dispatch!($vm, ip, $table)
    }};
}

fn r#return(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let result = vm.stack.pop().unwrap_or(Value::NULL);
    finish_return!(vm, result, ip, unit, table)
}

fn end(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    finish_return!(vm, Value::NULL, ip, unit, table)
}

fn load_local_return(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let slot = Chunk::inline_operand(unit as u16) as usize;
    let result = vm.stack[vm.threaded.base + slot];
    finish_return!(vm, result, ip, unit, table)
}

fn load_field_this_return(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let index = Chunk::inline_operand(unit as u16) as usize;
    let offset = vm.threaded.field_offset;
    let Some(result) = vm.field_of(vm.stack[vm.threaded.base], offset, index) else {
        become not_a_field(vm, ip, unit, table);
    };
    finish_return!(vm, result, ip, unit, table)
}

/// A return with open upvalues to close, or one that reaches this
/// invocation's floor: the `match` loop's return arm, whole. The value is in
/// `State::result`.
#[inline(never)]
fn return_slow(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let result = vm.threaded.result;
    let base = vm.threaded.base;
    vm.close_upvalues(base);
    vm.stack.truncate(base);
    vm.frames.pop();
    if vm.frames.len() <= vm.threaded.floor {
        go!(vm, leave_at_floor(vm, result, ip), table)
    } else {
        vm.push_reserved(result);
        let ip = attempt!(vm, resume(vm));
        dispatch!(vm, ip, table)
    }
}

/// A return into a frame whose chunk nobody parked; see `finish_return!`.
#[inline(never)]
fn resume_derived(vm: &mut Vm, _ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let ip = attempt!(vm, resume(vm));
    dispatch!(vm, ip, table)
}

/// A return that reached this invocation's floor: the fiber finishing into its
/// caller, or the invocation returning to Rust.
#[inline(never)]
fn leave_at_floor(vm: &mut Vm, result: Value, ip: *const u16) -> Next {
    let finished = vm.frames.is_empty();
    let caller = vm.current_fiber.and_then(|id| match vm.heap.fiber(id) {
        Some(fiber) => fiber.caller,
        _ => None,
    });
    if let (Some(caller), true) = (caller, finished) {
        let at = offset(vm, ip);
        let chunk = running(vm);
        next_or_fail!(
            vm,
            vm.perform_switch(
                Switch {
                    target: caller,
                    value: result,
                    set_caller: false,
                    catching: false,
                    finishing: true,
                    as_error: false,
                },
                at,
                chunk,
            )
        );
        return Ok(next_or_fail!(vm, resume(vm)));
    }
    if finished {
        if let Some(id) = vm.current_fiber {
            if let Some(fiber) = vm.heap.fiber_mut(id) {
                fiber.done = true;
            }
        }
    }
    vm.threaded.result = result;
    Err(Exit::Returned)
}

// ---------------------------------------------------------------------------
// Cold handlers: rare instructions, and the continuations hot handlers leave
// to. These may call whatever they like.
// ---------------------------------------------------------------------------

/// A field access on something that is not an instance.
#[inline(never)]
fn not_a_field(vm: &mut Vm, _ip: *const u16, _unit: u32, _table: &'static Table) -> Exit {
    fail!(vm, Vm::not_a_field());
}

fn load_upvalue(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let slot = Chunk::inline_operand(unit as u16) as usize;
    let value = attempt!(vm, vm.read_upvalue(vm.threaded.base, slot));
    vm.stack.push(value);
    dispatch!(vm, ip, table)
}

fn store_upvalue(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let slot = Chunk::inline_operand(unit as u16) as usize;
    let value = *vm.stack.last().unwrap();
    attempt!(vm, vm.write_upvalue(slot, value));
    dispatch!(vm, ip, table)
}

fn close_upvalue(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    vm.close_upvalues(vm.stack.len() - 1);
    vm.stack.pop();
    dispatch!(vm, ip, table)
}

#[allow(unsafe_code)]
fn closure(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let count = Chunk::inline_operand(unit as u16) as usize;
    let index = unsafe { *ip } as usize;
    let mut ip = unsafe { ip.add(1) };
    let Some(function) = vm.threaded.constants[index].as_object() else {
        fail!(vm, RuntimeError::new("Closure constant is not a function."));
    };
    let base = vm.threaded.base;
    let mut upvalues = Vec::with_capacity(count);
    for _ in 0..count {
        let descriptor = unsafe { *ip };
        let is_local = descriptor & 0xff != 0;
        let index = Chunk::inline_operand(descriptor) as usize;
        ip = unsafe { ip.add(1) };
        let captured = if is_local {
            vm.capture_upvalue(base + index)
        } else {
            attempt!(vm, vm.upvalue_of_current(index))
        };
        upvalues.push(captured);
    }
    let id = vm.heap.allocate(Object::Closure(ObjClosure { function, upvalues }));
    vm.stack.push(Value::object(id));
    collect_if_due!(vm, ip);
    dispatch!(vm, ip, table)
}

fn class(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let declared = Chunk::inline_operand(unit as u16) as usize;
    let superclass = vm.stack.pop().unwrap_or(Value::NULL);
    let name = vm.stack.pop().unwrap_or(Value::NULL);
    let class = attempt!(vm, vm.make_class(name, superclass, declared));
    vm.stack.push(class);
    collect_if_due!(vm, ip);
    dispatch!(vm, ip, table)
}

/// `MethodInstance` and `MethodStatic`, which differ only in where the method
/// lands.
#[inline(always)]
#[allow(unsafe_code)]
fn bind(vm: &mut Vm, ip: *const u16, is_static: bool) -> Result<*const u16, RuntimeError> {
    let symbol = unsafe { *ip } as usize;
    let ip = unsafe { ip.add(1) };
    let class = vm.stack.pop().unwrap_or(Value::NULL);
    let body = vm.stack.pop().unwrap_or(Value::NULL);
    vm.bind_method(class, body, symbol, is_static)?;
    Ok(ip)
}

fn method_instance(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let ip = attempt!(vm, bind(vm, ip, false));
    collect_if_due!(vm, ip);
    dispatch!(vm, ip, table)
}

fn method_static(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let ip = attempt!(vm, bind(vm, ip, true));
    collect_if_due!(vm, ip);
    dispatch!(vm, ip, table)
}

fn store_field_this(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let index = Chunk::inline_operand(unit as u16) as usize;
    let value = *vm.stack.last().unwrap();
    let receiver = vm.stack[vm.threaded.base];
    let offset = vm.threaded.field_offset;
    if !vm.set_field(receiver, offset, index, value) {
        fail!(vm, Vm::not_a_field());
    }
    dispatch!(vm, ip, table)
}

fn load_field(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let index = Chunk::inline_operand(unit as u16) as usize;
    let receiver = vm.stack.pop().unwrap_or(Value::NULL);
    let offset = vm.threaded.field_offset;
    let Some(value) = vm.field_of(receiver, offset, index) else {
        fail!(vm, Vm::not_a_field());
    };
    vm.stack.push(value);
    dispatch!(vm, ip, table)
}

fn store_field(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let index = Chunk::inline_operand(unit as u16) as usize;
    let value = vm.stack.pop().unwrap_or(Value::NULL);
    let receiver = vm.stack.pop().unwrap_or(Value::NULL);
    let offset = vm.threaded.field_offset;
    if !vm.set_field(receiver, offset, index, value) {
        fail!(vm, Vm::not_a_field());
    }
    vm.stack.push(value);
    dispatch!(vm, ip, table)
}

fn load_static_field(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let index = Chunk::inline_operand(unit as u16) as usize;
    let value = vm.static_field(index);
    vm.stack.push(value);
    dispatch!(vm, ip, table)
}

fn store_static_field(vm: &mut Vm, ip: *const u16, unit: u32, table: &'static Table) -> Exit {
    let index = Chunk::inline_operand(unit as u16) as usize;
    let value = *vm.stack.last().unwrap();
    attempt!(vm, vm.set_static_field(index, value));
    collect_if_due!(vm, ip);
    dispatch!(vm, ip, table)
}

fn set_attributes(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    vm.set_attributes();
    collect_if_due!(vm, ip);
    dispatch!(vm, ip, table)
}

#[allow(unsafe_code)]
fn import_module(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let index = unsafe { *ip } as usize;
    let ip = unsafe { ip.add(1) };
    let name = vm.to_string(vm.threaded.constants[index]);
    let module = vm.threaded.module;
    let name = super::resolve_module(&vm.modules[module].name, &name);

    match vm.load_module(&name) {
        Ok((_, None)) => {
            vm.stack.push(Value::NULL);
            collect_if_due!(vm, ip);
            dispatch!(vm, ip, table)
        }
        Ok((_, Some(closure))) => {
            // The module body runs as an ordinary call, its result landing
            // where this instruction's would have.
            let body_base = vm.stack.len();
            vm.stack.push(Value::NULL);
            let at = offset(vm, ip);
            let chunk = running(vm);
            if let Some(frame) = vm.frames.last_mut() {
                frame.ip = at;
                frame.chunk = Some(chunk);
            }
            let next_chunk = attempt!(vm, vm.push_frame(closure, body_base));
            let ip = enter(vm, next_chunk);
            dispatch!(vm, ip, table)
        }
        Err(error) => {
            let at = offset(vm, ip);
            go!(vm, delivered(vm, error, at), table)
        }
    }
}

#[allow(unsafe_code)]
fn import_variable(vm: &mut Vm, ip: *const u16, _unit: u32, table: &'static Table) -> Exit {
    let module_name = unsafe { *ip } as usize;
    let variable_name = unsafe { *ip.add(1) } as usize;
    let line = line_of(vm, ip);
    let ip = unsafe { ip.add(2) };
    let module_name = vm.to_string(vm.threaded.constants[module_name]);
    let module = vm.threaded.module;
    let module_name = super::resolve_module(&vm.modules[module].name, &module_name);
    let variable = vm.to_string(vm.threaded.constants[variable_name]);
    let value = attempt!(vm, vm.imported_variable(&module_name, &variable, line));
    vm.stack.push(value);
    collect_if_due!(vm, ip);
    dispatch!(vm, ip, table)
}

/// The 24 table entries past the last opcode. Unreachable for any chunk that
/// can exist -- see `code::HIGHEST` -- and an error rather than undefined
/// behaviour if one ever is.
fn bad_opcode(vm: &mut Vm, ip: *const u16, unit: u32, _table: &'static Table) -> Exit {
    let line = line_of(vm, ip);
    fail!(vm, Vm::bad_opcode(Chunk::opcode_of(unit as u16), line));
}
