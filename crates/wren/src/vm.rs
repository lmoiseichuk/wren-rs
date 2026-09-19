//! The interpreter loop.
//!
//! One `Vm` owns the heap, the value stack, the method-symbol table and the
//! module's variables. [`Vm::interpret`] compiles a string and runs it, which
//! is upstream's `wrenInterpret` in the same shape.
//!
//! # What is not here yet
//!
//! Call frames. There are no user-defined functions, methods or fibers, so
//! every program runs in one frame and `Op::Return` ends it. The loop is
//! written so that adding frames is adding a stack of them, not restructuring
//! what is here — but it is not pretending to have them.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::bytecode::{Chunk, Op};
use crate::compiler;
use crate::core;
use crate::handle::ObjectId;
use crate::heap::Heap;
use crate::math;
use crate::object::{
    Method, ObjClass, ObjClosure, ObjFn, ObjInstance, ObjString, ObjUpvalue, Object,
};
use crate::symbol::SymbolTable;
use crate::value::Value;

/// A runtime failure, with the line it happened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub message: String,
    pub line: u16,
}

impl RuntimeError {
    pub fn new(message: impl Into<String>) -> RuntimeError {
        // The line is filled in by the interpreter loop, which is the only
        // place that knows where it is. A primitive raising an error does not
        // have to thread one through.
        RuntimeError { message: message.into(), line: 0 }
    }
}

/// Anything that can go wrong interpreting a source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrenError {
    /// The source did not compile. Upstream's `WREN_RESULT_COMPILE_ERROR`.
    Compile { message: String, line: u16 },
    /// The program ran and failed. Upstream's `WREN_RESULT_RUNTIME_ERROR`.
    Runtime(RuntimeError),
}

impl WrenError {
    pub fn message(&self) -> &str {
        match self {
            WrenError::Compile { message, .. } => message,
            WrenError::Runtime(error) => &error.message,
        }
    }

    pub fn line(&self) -> u16 {
        match self {
            WrenError::Compile { line, .. } => *line,
            WrenError::Runtime(error) => error.line,
        }
    }
}

/// How deep calls may nest before it is called a stack overflow.
///
/// **Upstream has no such limit** — it grows the frame array until the process
/// runs out. On a part with 8 KB that is a reboot rather than an error, and an
/// infinite recursion is a thing programs do. A number here turns a crash into
/// a message.
const MAX_FRAMES: usize = 256;

/// One call in progress.
pub struct Frame {
    pub closure: ObjectId,
    /// Where to resume. Only written when this frame stops being the running
    /// one; while it runs, the interpreter keeps it in a local.
    pub ip: usize,
    /// The stack slot holding the receiver. Local slot *n* is `base + n`, and
    /// slot 0 is `this`.
    pub base: usize,
}

/// A module's variables: names and values, in parallel.
pub struct Module {
    pub names: SymbolTable,
    pub values: Vec<Value>,
}

impl Module {
    fn new() -> Module {
        Module { names: SymbolTable::new(), values: Vec::new() }
    }

    /// Define a variable, or return its existing slot.
    pub fn define(&mut self, name: &str, value: Value) -> usize {
        let index = self.names.ensure(name);
        if self.values.len() <= index {
            self.values.resize(index + 1, Value::NULL);
        }
        self.values[index] = value;
        index
    }
}

/// The virtual machine.
pub struct Vm {
    pub heap: Heap,
    /// The value stack. Locals live here too, indexed from the frame base —
    /// which is zero, there being one frame.
    pub stack: Vec<Value>,
    /// Method signatures, interned. `Op::Call` carries an index into this.
    pub method_names: SymbolTable,
    pub module: Module,

    /// The built-in classes, held so that [`Vm::class_of`] can answer without
    /// a lookup. Upstream keeps exactly the same set on its `WrenVM`.
    pub num_class: ObjectId,
    pub bool_class: ObjectId,
    pub null_class: ObjectId,
    pub string_class: ObjectId,
    pub list_class: ObjectId,
    pub map_class: ObjectId,
    pub range_class: ObjectId,
    pub class_class: ObjectId,
    /// The root of the hierarchy. Every class inherits from it, and the method
    /// lookup walks up to it before giving up.
    pub object_class: ObjectId,
    /// The class of every function and closure, holding `call`.
    pub fn_class: ObjectId,
    /// What iterating a map yields: a `key`/`value` pair.
    pub map_entry_class: ObjectId,

    /// Calls in progress, innermost last.
    pub frames: Vec<Frame>,
    /// Upvalues still pointing at live stack slots. See
    /// [`ObjUpvalue`](crate::object::ObjUpvalue).
    open_upvalues: Vec<ObjectId>,

    /// Where `System.print` writes.
    ///
    /// A buffer rather than a writer, for now. A port would replace this with
    /// something that reaches a UART; keeping it a `Vec<u8>` is what lets the
    /// tests assert on output without a mock, and it is the smaller thing to
    /// change later.
    pub output: Vec<u8>,
}

impl Vm {
    pub fn new() -> Vm {
        let mut heap = Heap::new();

        // The classes have to exist before anything can be dispatched on, and
        // they refer to their own names, so the names are allocated first.
        let class_named = |heap: &mut Heap, name: &str, superclass: Option<ObjectId>| {
            let name = heap.allocate(Object::String(ObjString::from_text(name)));
            heap.allocate(Object::Class(alloc::boxed::Box::new(
                crate::object::ObjClass::new(name, superclass),
            )))
        };

        // **`Object` is the root, and every other class inherits from it.**
        // That is not decoration: `!` is defined there and returns `false` for
        // everything, which is how `!0` and `!""` are `false` in Wren while
        // `Bool` and `Null` override it. Without the root, those are a missing
        // method rather than an answer.
        let object_class = class_named(&mut heap, "Object", None);
        let root = Some(object_class);

        let num_class = class_named(&mut heap, "Num", root);
        let bool_class = class_named(&mut heap, "Bool", root);
        let null_class = class_named(&mut heap, "Null", root);
        let string_class = class_named(&mut heap, "String", root);
        let list_class = class_named(&mut heap, "List", root);
        let map_class = class_named(&mut heap, "Map", root);
        let range_class = class_named(&mut heap, "Range", root);
        let class_class = class_named(&mut heap, "Class", root);
        let fn_class = class_named(&mut heap, "Fn", root);
        let map_entry_class = class_named(&mut heap, "MapEntry", root);

        let mut vm = Vm {
            heap,
            stack: Vec::new(),
            method_names: SymbolTable::new(),
            module: Module::new(),
            num_class,
            bool_class,
            null_class,
            string_class,
            list_class,
            map_class,
            range_class,
            class_class,
            object_class,
            fn_class,
            map_entry_class,
            frames: Vec::new(),
            open_upvalues: Vec::new(),
            output: Vec::new(),
        };
        core::install(&mut vm);
        vm
    }

    /// Compile `source` and run it.
    pub fn interpret(&mut self, source: &str) -> Result<(), WrenError> {
        let chunk = compiler::compile(self, source)
            .map_err(|error| WrenError::Compile { message: error.message, line: error.line })?;
        self.run(Rc::new(chunk)).map_err(WrenError::Runtime)
    }

    /// What `System.print` has written so far.
    pub fn output_str(&self) -> &str {
        ::core::str::from_utf8(&self.output).unwrap_or("<not utf-8>")
    }

    /// The class of a value, as a handle.
    ///
    /// For everything but a class or an instance this is implied by the
    /// variant, which is why the objects themselves carry no class pointer —
    /// see the note on `classObj` in the crate README.
    pub fn class_of(&self, value: Value) -> Option<ObjectId> {
        if value.is_num() {
            return Some(self.num_class);
        }
        if value.is_bool() {
            return Some(self.bool_class);
        }
        if value.is_null() {
            return Some(self.null_class);
        }
        let id = value.as_object()?;
        match self.heap.get(id)? {
            Object::String(_) => Some(self.string_class),
            Object::List(_) => Some(self.list_class),
            Object::Map(_) => Some(self.map_class),
            Object::Range(_) => Some(self.range_class),
            // A class's own class is its metaclass, which is what makes a
            // static call like `System.print` land on the right table.
            Object::Class(class) => Some(class.metaclass.unwrap_or(self.class_class)),
            Object::Instance(instance) => Some(instance.class),
            Object::Fn(_) | Object::Closure(_) => Some(self.fn_class),
            // An upvalue is never a value a program can hold; it exists only
            // inside a closure.
            Object::Upvalue(_) => None,
        }
    }

    /// Find a method, walking up the superclass chain.
    fn find_method(&self, class: ObjectId, symbol: usize) -> Option<Method> {
        let mut current = Some(class);
        while let Some(id) = current {
            let Some(Object::Class(class)) = self.heap.get(id) else {
                return None;
            };
            if let Some(method) = class.method(symbol) {
                return Some(method);
            }
            current = class.superclass;
        }
        None
    }

    /// Allocate a string and return a value referring to it.
    pub fn new_string(&mut self, text: &str) -> Value {
        Value::object(self.heap.allocate(Object::String(ObjString::from_text(text))))
    }

    /// Read a string object, for a primitive that needs its contents.
    pub fn string_at(&self, value: Value) -> Option<&str> {
        match self.heap.get(value.as_object()?)? {
            Object::String(text) => text.as_str(),
            _ => None,
        }
    }

    /// Wren's `toString` for the values that have one without dispatching.
    ///
    /// Numbers print as integers when they are integral — `1`, not `1.0` —
    /// which is upstream's behaviour and the single most visible formatting
    /// rule in the language.
    pub fn to_string(&self, value: Value) -> String {
        if let Some(number) = value.as_num() {
            if number == math::trunc(number) && number.is_finite() && math::abs(number) < 1e21 {
                return format!("{}", number as i64);
            }
            return format!("{number}");
        }
        if value.is_null() {
            return "null".to_string();
        }
        if value.is_true() {
            return "true".to_string();
        }
        if value.is_false() {
            return "false".to_string();
        }
        let Some(id) = value.as_object() else {
            return "<invalid>".to_string();
        };
        match self.heap.get(id) {
            Some(Object::String(text)) => text.as_str().unwrap_or("<not utf-8>").to_string(),
            Some(Object::Range(range)) => {
                let separator = if range.is_inclusive { ".." } else { "..." };
                format!(
                    "{}{}{}",
                    self.to_string(Value::num(range.from)),
                    separator,
                    self.to_string(Value::num(range.to))
                )
            }
            Some(Object::List(list)) => {
                let mut out = String::from("[");
                for (at, element) in list.elements.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&self.to_string(*element));
                }
                out.push(']');
                out
            }
            Some(Object::Map(map)) => format!("<map {}>", map.entries.len()),
            Some(Object::Class(class)) => match self.heap.get(class.name) {
                Some(Object::String(name)) => name.as_str().unwrap_or("<class>").to_string(),
                _ => "<class>".to_string(),
            },
            Some(Object::Instance(instance)) => {
                format!("instance of {}", self.class_name(instance.class))
            }
            Some(Object::Fn(_)) | Some(Object::Closure(_)) => "<fn>".to_string(),
            Some(Object::Upvalue(_)) => "<upvalue>".to_string(),
            None => "<collected>".to_string(),
        }
    }

    /// Push a frame for a closure whose receiver and arguments are already on
    /// the stack, with the receiver at `base`.
    fn push_frame(&mut self, closure: ObjectId, base: usize) -> Result<Rc<Chunk>, RuntimeError> {
        if self.frames.len() >= MAX_FRAMES {
            return Err(RuntimeError::new("Stack overflow."));
        }
        let chunk = self.chunk_of(closure)?;
        self.frames.push(Frame { closure, ip: 0, base });
        Ok(chunk)
    }

    fn chunk_of(&self, closure: ObjectId) -> Result<Rc<Chunk>, RuntimeError> {
        let Some(Object::Closure(closure)) = self.heap.get(closure) else {
            return Err(RuntimeError::new("Not a closure."));
        };
        let Some(Object::Fn(function)) = self.heap.get(closure.function) else {
            return Err(RuntimeError::new("Closure has no function."));
        };
        Ok(function.chunk.clone())
    }

    /// How many arguments a closure takes.
    pub fn arity_of(&self, closure: ObjectId) -> Option<usize> {
        self.function_of(closure).map(|function| function.arity)
    }

    fn function_of(&self, closure: ObjectId) -> Option<&ObjFn> {
        let Some(Object::Closure(closure)) = self.heap.get(closure) else {
            return None;
        };
        match self.heap.get(closure.function) {
            Some(Object::Fn(function)) => Some(function),
            _ => None,
        }
    }

    /// Capture a stack slot as an upvalue, reusing one if it is already open.
    ///
    /// **Reuse is not an optimisation, it is the semantics.** Two closures over
    /// the same variable must see each other's assignments; giving each its own
    /// upvalue would silently turn one shared variable into two.
    fn capture_upvalue(&mut self, slot: usize) -> ObjectId {
        for existing in &self.open_upvalues {
            if let Some(Object::Upvalue(upvalue)) = self.heap.get(*existing) {
                if upvalue.closed.is_none() && upvalue.slot == slot {
                    return *existing;
                }
            }
        }
        let id = self.heap.allocate(Object::Upvalue(ObjUpvalue { slot, closed: None }));
        self.open_upvalues.push(id);
        id
    }

    /// Close every upvalue at or above `from`, copying the stack slot into it.
    ///
    /// Called when a frame returns or a scope with captured locals ends: the
    /// slots are about to be reused, so anything still referring to them has to
    /// take its own copy first.
    fn close_upvalues(&mut self, from: usize) {
        let mut still_open = Vec::new();
        for id in ::core::mem::take(&mut self.open_upvalues) {
            let slot = match self.heap.get(id) {
                Some(Object::Upvalue(upvalue)) if upvalue.closed.is_none() => upvalue.slot,
                _ => continue,
            };
            if slot < from {
                still_open.push(id);
                continue;
            }
            let value = self.stack.get(slot).copied().unwrap_or(Value::NULL);
            if let Some(Object::Upvalue(upvalue)) = self.heap.get_mut(id) {
                upvalue.closed = Some(value);
            }
        }
        self.open_upvalues = still_open;
    }

    /// Every value the collector must treat as reachable.
    fn roots(&self) -> Vec<Value> {
        let mut roots = self.stack.clone();
        roots.extend(self.module.values.iter().copied());
        for frame in &self.frames {
            roots.push(Value::object(frame.closure));
        }
        for id in &self.open_upvalues {
            roots.push(Value::object(*id));
        }
        // The core classes are reachable from nothing else once a program has
        // stopped mentioning them by name, and freeing `Num` mid-program would
        // be spectacular.
        for class in [
            self.num_class, self.bool_class, self.null_class, self.string_class,
            self.list_class, self.map_class, self.range_class, self.class_class,
            self.object_class, self.fn_class, self.map_entry_class,
        ] {
            roots.push(Value::object(class));
        }
        roots
    }

    /// A value as text, dispatching `toString` so a class's own conversion is
    /// used when it has one.
    pub fn stringify(&mut self, value: Value) -> Result<String, RuntimeError> {
        let text = self.invoke(value, "toString")?;
        Ok(self.to_string(text))
    }

    /// Call a method on a value from Rust, running it to completion.
    ///
    /// **This is what makes a user-defined `toString` work.** `System.print`
    /// cannot simply format the value, because a class may define its own
    /// conversion and that is the whole point of defining one. Falling back to
    /// the value itself when there is no such method keeps this usable for
    /// optional protocol methods.
    pub fn invoke(&mut self, receiver: Value, signature: &str) -> Result<Value, RuntimeError> {
        let Some(symbol) = self.method_names.find(signature) else {
            return Ok(receiver);
        };
        let Some(class) = self.class_of(receiver) else {
            return Ok(receiver);
        };
        let Some(method) = self.find_method(class, symbol) else {
            return Ok(receiver);
        };

        let at = self.stack.len();
        self.stack.push(receiver);
        let result = match method {
            Method::Primitive(function) => function(self, at),
            Method::Closure(closure) => self.call_closure(closure, at),
        };
        self.stack.truncate(at);
        result
    }

    /// Call a Wren function with arguments, from Rust.
    ///
    /// This is what a core method written in Rust needs in order to take a
    /// block: `list.map { ... }` has to actually run the block once per
    /// element. The receiver slot holds the function itself, which is what a
    /// closure's slot zero is.
    pub fn call_function(&mut self, function: Value, args: &[Value]) -> Result<Value, RuntimeError> {
        let Some(closure) = function.as_object() else {
            return Err(RuntimeError::new("Argument must be a function."));
        };
        if !matches!(self.heap.get(closure), Some(Object::Closure(_))) {
            return Err(RuntimeError::new("Argument must be a function."));
        }
        let at = self.stack.len();
        self.stack.push(function);
        for argument in args {
            self.stack.push(*argument);
        }
        let result = self.call_closure(closure, at);
        self.stack.truncate(at);
        result
    }

    /// Call a closure from a primitive, running it to completion.
    ///
    /// This is what lets a core method written in Rust take a Wren function —
    /// `list.map { ... }` and friends. The receiver and arguments must already
    /// be on the stack at `base`.
    pub fn call_closure(&mut self, closure: ObjectId, base: usize) -> Result<Value, RuntimeError> {
        let depth = self.frames.len();
        let chunk = self.push_frame(closure, base)?;
        self.run_frames(chunk, depth)
    }

    /// Run a compiled chunk as a fresh top-level frame.
    pub fn run(&mut self, chunk: Rc<Chunk>) -> Result<(), RuntimeError> {
        // Module code is a function like any other, so that one loop handles
        // both and `return` at the top level means the same thing it does
        // anywhere else.
        let function = self.heap.allocate(Object::Fn(Box::new(ObjFn {
            chunk: chunk.clone(),
            arity: 0,
            num_upvalues: 0,
            name: "(module)".into(),
            field_offset: 0,
            super_class: None,
        })));
        let closure = self
            .heap
            .allocate(Object::Closure(Box::new(ObjClosure { function, upvalues: Vec::new() })));

        let base = self.stack.len();
        self.stack.push(Value::NULL);
        let depth = self.frames.len();
        let chunk = self.push_frame(closure, base)?;
        self.run_frames(chunk, depth)?;
        Ok(())
    }

    /// The interpreter loop.
    ///
    /// `chunk`, `ip` and `base` are kept in locals rather than read out of the
    /// frame each time: the fetch is the hottest path there is, and going
    /// through `self.frames.last()` for every instruction costs a bounds check
    /// and an indirection per opcode. They are written back to the frame only
    /// when a call or a return changes which frame is running.
    fn run_frames(&mut self, mut chunk: Rc<Chunk>, floor: usize) -> Result<Value, RuntimeError> {
        let mut ip = self.frames.last().map_or(0, |frame| frame.ip);
        let mut base = self.frames.last().map_or(0, |frame| frame.base);

        loop {
            let byte = chunk.code[ip];
            let line = chunk.line_at(ip);
            ip += 1;

            let Some(op) = Op::from_byte(byte) else {
                return Err(RuntimeError { message: format!("bad opcode {byte}"), line });
            };

            match op {
                Op::Constant => {
                    let index = chunk.read_short(ip) as usize;
                    ip += 2;
                    self.stack.push(chunk.constants[index]);
                }
                Op::Null => self.stack.push(Value::NULL),
                Op::False => self.stack.push(Value::FALSE),
                Op::True => self.stack.push(Value::TRUE),
                Op::LoadLocal => {
                    let slot = chunk.code[ip] as usize;
                    ip += 1;
                    self.stack.push(self.stack[base + slot]);
                }
                Op::StoreLocal => {
                    let slot = chunk.code[ip] as usize;
                    ip += 1;
                    self.stack[base + slot] = *self.stack.last().unwrap();
                }
                Op::LoadModuleVar => {
                    let index = chunk.read_short(ip) as usize;
                    ip += 2;
                    self.stack.push(self.module.values[index]);
                }
                Op::StoreModuleVar => {
                    let index = chunk.read_short(ip) as usize;
                    ip += 2;
                    self.module.values[index] = *self.stack.last().unwrap();
                }
                Op::Pop => {
                    self.stack.pop();
                }
                Op::LoadUpvalue => {
                    let slot = chunk.code[ip] as usize;
                    ip += 1;
                    let value = self.read_upvalue(base, slot)?;
                    self.stack.push(value);
                }
                Op::StoreUpvalue => {
                    let slot = chunk.code[ip] as usize;
                    ip += 1;
                    let value = *self.stack.last().unwrap();
                    self.write_upvalue(slot, value)?;
                }
                Op::CloseUpvalue => {
                    self.close_upvalues(self.stack.len() - 1);
                    self.stack.pop();
                }
                Op::Closure => {
                    let index = chunk.read_short(ip) as usize;
                    ip += 2;
                    let function = chunk.constants[index]
                        .as_object()
                        .ok_or_else(|| RuntimeError::new("Closure constant is not a function."))?;
                    let count = chunk.code[ip] as usize;
                    ip += 1;

                    let mut upvalues = Vec::with_capacity(count);
                    for _ in 0..count {
                        let is_local = chunk.code[ip] != 0;
                        let index = chunk.code[ip + 1] as usize;
                        ip += 2;
                        let captured = if is_local {
                            self.capture_upvalue(base + index)
                        } else {
                            self.upvalue_of_current(index)?
                        };
                        upvalues.push(captured);
                    }

                    let id = self
                        .heap
                        .allocate(Object::Closure(Box::new(ObjClosure { function, upvalues })));
                    self.stack.push(Value::object(id));
                }
                Op::Class => {
                    let declared = chunk.code[ip] as usize;
                    ip += 1;
                    let superclass = self.stack.pop().unwrap_or(Value::NULL);
                    let name = self.stack.pop().unwrap_or(Value::NULL);
                    let class = self.make_class(name, superclass, declared)?;
                    self.stack.push(class);
                }
                Op::MethodInstance | Op::MethodStatic => {
                    let symbol = chunk.read_short(ip) as usize;
                    ip += 2;
                    let class = self.stack.pop().unwrap_or(Value::NULL);
                    let body = self.stack.pop().unwrap_or(Value::NULL);
                    self.bind_method(class, body, symbol, op == Op::MethodStatic)?;
                }
                Op::LoadFieldThis => {
                    let index = chunk.code[ip] as usize;
                    ip += 1;
                    let value = self.field_of(self.stack[base], base, index)?;
                    self.stack.push(value);
                }
                Op::StoreFieldThis => {
                    let index = chunk.code[ip] as usize;
                    ip += 1;
                    let value = *self.stack.last().unwrap();
                    let receiver = self.stack[base];
                    self.set_field(receiver, base, index, value)?;
                }
                Op::LoadField => {
                    let index = chunk.code[ip] as usize;
                    ip += 1;
                    let receiver = self.stack.pop().unwrap_or(Value::NULL);
                    let value = self.field_of(receiver, base, index)?;
                    self.stack.push(value);
                }
                Op::StoreField => {
                    let index = chunk.code[ip] as usize;
                    ip += 1;
                    let receiver = self.stack.pop().unwrap_or(Value::NULL);
                    let value = *self.stack.last().unwrap();
                    self.set_field(receiver, base, index, value)?;
                }
                Op::Construct => {
                    let class = self.stack[base];
                    let instance = self.instantiate(class)?;
                    self.stack[base] = instance;
                }
                Op::Call | Op::Super => {
                    let arity = chunk.code[ip] as usize;
                    ip += 1;
                    let symbol = chunk.read_short(ip) as usize;
                    ip += 2;

                    let start_from = if op == Op::Super {
                        self.frames
                            .last()
                            .and_then(|frame| self.function_of(frame.closure))
                            .and_then(|function| function.super_class)
                            .ok_or_else(|| {
                                RuntimeError::new("Cannot use 'super' outside of a method.")
                            })?
                    } else {
                        let receiver_at = self.stack.len() - arity - 1;
                        let receiver = self.stack[receiver_at];
                        self.class_of(receiver)
                            .ok_or_else(|| RuntimeError::new("Receiver has no class."))?
                    };

                    let receiver_at = self.stack.len() - arity - 1;
                    let Some(method) = self.find_method(start_from, symbol) else {
                        let name = self.method_names.name(symbol).unwrap_or("?").to_string();
                        let receiver = self.stack[receiver_at];
                        let class_name = self
                            .class_of(receiver)
                            .map(|class| self.class_name(class))
                            .unwrap_or_else(|| "null".to_string());
                        return Err(RuntimeError {
                            message: format!("{class_name} does not implement '{name}'."),
                            line,
                        });
                    };

                    match method {
                        Method::Primitive(function) => {
                            let value = function(self, receiver_at).map_err(|mut error| {
                                if error.line == 0 {
                                    error.line = line;
                                }
                                error
                            })?;
                            self.stack.truncate(receiver_at);
                            self.stack.push(value);
                        }
                        Method::Closure(closure) => {
                            let wanted = self
                                .function_of(closure)
                                .map(|function| function.arity)
                                .unwrap_or(0);
                            if wanted != arity {
                                return Err(RuntimeError {
                                    message: format!(
                                        "Function expects {wanted} argument(s) but got {arity}."
                                    ),
                                    line,
                                });
                            }
                            if let Some(frame) = self.frames.last_mut() {
                                frame.ip = ip;
                            }
                            chunk = self.push_frame(closure, receiver_at)?;
                            ip = 0;
                            base = receiver_at;
                        }
                    }
                }
                Op::Return | Op::End => {
                    let result = if op == Op::End {
                        Value::NULL
                    } else {
                        self.stack.pop().unwrap_or(Value::NULL)
                    };

                    self.close_upvalues(base);
                    self.stack.truncate(base);
                    self.frames.pop();

                    if self.frames.len() <= floor {
                        return Ok(result);
                    }

                    self.stack.push(result);
                    let frame = self.frames.last().unwrap();
                    ip = frame.ip;
                    base = frame.base;
                    let closure = frame.closure;
                    chunk = self.chunk_of(closure)?;
                }
                Op::Jump => {
                    let offset = chunk.read_short(ip) as usize;
                    ip += 2 + offset;
                }
                Op::Loop => {
                    let offset = chunk.read_short(ip) as usize;
                    ip += 2;
                    ip -= offset;
                }
                Op::JumpIf => {
                    let offset = chunk.read_short(ip) as usize;
                    ip += 2;
                    let condition = self.stack.pop().unwrap_or(Value::NULL);
                    if condition.is_falsy() {
                        ip += offset;
                    }
                }
                Op::And => {
                    let offset = chunk.read_short(ip) as usize;
                    ip += 2;
                    if self.stack.last().copied().unwrap_or(Value::NULL).is_falsy() {
                        ip += offset;
                    } else {
                        self.stack.pop();
                    }
                }
                Op::Or => {
                    let offset = chunk.read_short(ip) as usize;
                    ip += 2;
                    if self.stack.last().copied().unwrap_or(Value::NULL).is_falsy() {
                        self.stack.pop();
                    } else {
                        ip += offset;
                    }
                }
            }

            // Collect between instructions, where the roots are exactly the
            // stack, the module and the frames -- never part way through
            // building something.
            if self.heap.should_collect() {
                if let Some(frame) = self.frames.last_mut() {
                    frame.ip = ip;
                }
                let roots = self.roots();
                self.heap.collect(roots);
            }
        }
    }

    fn read_upvalue(&self, base: usize, slot: usize) -> Result<Value, RuntimeError> {
        let _ = base;
        let Some(frame) = self.frames.last() else {
            return Err(RuntimeError::new("No frame."));
        };
        let Some(Object::Closure(closure)) = self.heap.get(frame.closure) else {
            return Err(RuntimeError::new("No closure."));
        };
        let Some(id) = closure.upvalues.get(slot).copied() else {
            return Err(RuntimeError::new("No such upvalue."));
        };
        match self.heap.get(id) {
            Some(Object::Upvalue(upvalue)) => match upvalue.closed {
                Some(value) => Ok(value),
                None => Ok(self.stack.get(upvalue.slot).copied().unwrap_or(Value::NULL)),
            },
            _ => Err(RuntimeError::new("Not an upvalue.")),
        }
    }

    fn write_upvalue(&mut self, slot: usize, value: Value) -> Result<(), RuntimeError> {
        let Some(frame) = self.frames.last() else {
            return Err(RuntimeError::new("No frame."));
        };
        let Some(Object::Closure(closure)) = self.heap.get(frame.closure) else {
            return Err(RuntimeError::new("No closure."));
        };
        let Some(id) = closure.upvalues.get(slot).copied() else {
            return Err(RuntimeError::new("No such upvalue."));
        };
        let target = match self.heap.get(id) {
            Some(Object::Upvalue(upvalue)) => upvalue.closed.map(|_| None).unwrap_or(Some(upvalue.slot)),
            _ => return Err(RuntimeError::new("Not an upvalue.")),
        };
        match target {
            Some(stack_slot) => {
                if stack_slot < self.stack.len() {
                    self.stack[stack_slot] = value;
                }
            }
            None => {
                if let Some(Object::Upvalue(upvalue)) = self.heap.get_mut(id) {
                    upvalue.closed = Some(value);
                }
            }
        }
        Ok(())
    }

    fn upvalue_of_current(&self, index: usize) -> Result<ObjectId, RuntimeError> {
        let Some(frame) = self.frames.last() else {
            return Err(RuntimeError::new("No frame."));
        };
        let Some(Object::Closure(closure)) = self.heap.get(frame.closure) else {
            return Err(RuntimeError::new("No closure."));
        };
        closure
            .upvalues
            .get(index)
            .copied()
            .ok_or_else(|| RuntimeError::new("No such upvalue."))
    }

    fn class_name(&self, class: ObjectId) -> String {
        match self.heap.get(class) {
            Some(Object::Class(class)) => match self.heap.get(class.name) {
                Some(Object::String(name)) => name.as_str().unwrap_or("?").to_string(),
                _ => "?".to_string(),
            },
            _ => "?".to_string(),
        }
    }

    /// The field offset the currently running method was bound with.
    fn field_of(&self, receiver: Value, _base: usize, index: usize) -> Result<Value, RuntimeError> {
        let offset = self.current_field_offset();
        let Some(id) = receiver.as_object() else {
            return Err(RuntimeError::new("Cannot access a field outside of a class."));
        };
        match self.heap.get(id) {
            Some(Object::Instance(instance)) => {
                Ok(instance.fields.get(offset + index).copied().unwrap_or(Value::NULL))
            }
            _ => Err(RuntimeError::new("Cannot access a field outside of a class.")),
        }
    }

    fn set_field(
        &mut self,
        receiver: Value,
        _base: usize,
        index: usize,
        value: Value,
    ) -> Result<(), RuntimeError> {
        let offset = self.current_field_offset();
        let Some(id) = receiver.as_object() else {
            return Err(RuntimeError::new("Cannot access a field outside of a class."));
        };
        match self.heap.get_mut(id) {
            Some(Object::Instance(instance)) => {
                let at = offset + index;
                if instance.fields.len() <= at {
                    instance.fields.resize(at + 1, Value::NULL);
                }
                instance.fields[at] = value;
                Ok(())
            }
            _ => Err(RuntimeError::new("Cannot access a field outside of a class.")),
        }
    }

    fn current_field_offset(&self) -> usize {
        self.frames
            .last()
            .and_then(|frame| self.function_of(frame.closure))
            .map_or(0, |function| function.field_offset)
    }

    fn make_class(
        &mut self,
        name: Value,
        superclass: Value,
        declared: usize,
    ) -> Result<Value, RuntimeError> {
        let Some(superclass_id) = superclass.as_object() else {
            return Err(RuntimeError::new("Class must inherit from a class."));
        };
        let inherited = match self.heap.get(superclass_id) {
            Some(Object::Class(class)) => class.num_fields.max(0) as usize,
            _ => return Err(RuntimeError::new("Class must inherit from a class.")),
        };

        let Some(name_id) = name.as_object() else {
            return Err(RuntimeError::new("Class name must be a string."));
        };

        // A metaclass, so the class can carry static methods and a constructor.
        let metaclass_name = self.heap.allocate(Object::String(ObjString::from_text("metaclass")));
        let metaclass = self.heap.allocate(Object::Class(Box::new(ObjClass::new(
            metaclass_name,
            Some(self.class_class),
        ))));

        let mut class = ObjClass::new(name_id, Some(superclass_id));
        class.num_fields = (inherited + declared) as i32;
        class.metaclass = Some(metaclass);
        Ok(Value::object(self.heap.allocate(Object::Class(Box::new(class)))))
    }

    fn bind_method(
        &mut self,
        class: Value,
        body: Value,
        symbol: usize,
        is_static: bool,
    ) -> Result<(), RuntimeError> {
        let Some(class_id) = class.as_object() else {
            return Err(RuntimeError::new("Not a class."));
        };
        let Some(closure) = body.as_object() else {
            return Err(RuntimeError::new("Method body is not a closure."));
        };

        // **Where the field offset is filled in.** The compiler numbered this
        // method's fields from zero; now that the class is known, so is how
        // many fields the superclass already occupies.
        let inherited = match self.heap.get(class_id) {
            Some(Object::Class(class)) => match class.superclass {
                Some(superclass) => match self.heap.get(superclass) {
                    Some(Object::Class(superclass)) => superclass.num_fields.max(0) as usize,
                    _ => 0,
                },
                None => 0,
            },
            _ => 0,
        };
        let superclass = match self.heap.get(class_id) {
            Some(Object::Class(class)) => class.superclass,
            _ => None,
        };
        self.set_field_offset(closure, inherited, superclass);

        let target = if is_static {
            match self.heap.get(class_id) {
                Some(Object::Class(class)) => class.metaclass.unwrap_or(class_id),
                _ => class_id,
            }
        } else {
            class_id
        };

        if let Some(Object::Class(class)) = self.heap.get_mut(target) {
            class.define(symbol, Method::Closure(closure));
        }
        Ok(())
    }

    /// Set a closure's field offset, and every function nested inside it.
    ///
    /// A closure written inside a method still refers to the same fields, so
    /// the offset has to reach it too — upstream walks nested functions at bind
    /// time for the same reason.
    fn set_field_offset(&mut self, closure: ObjectId, offset: usize, superclass: Option<ObjectId>) {
        let Some(Object::Closure(closure)) = self.heap.get(closure) else {
            return;
        };
        let function = closure.function;
        let nested: Vec<ObjectId> = match self.heap.get(function) {
            Some(Object::Fn(function)) => function
                .chunk
                .constants
                .iter()
                .filter_map(|constant| constant.as_object())
                .filter(|id| matches!(self.heap.get(*id), Some(Object::Fn(_))))
                .collect(),
            _ => Vec::new(),
        };
        if let Some(Object::Fn(function)) = self.heap.get_mut(function) {
            function.field_offset = offset;
            function.super_class = superclass;
        }
        for id in nested {
            self.set_field_offset_of_fn(id, offset, superclass);
        }
    }

    fn set_field_offset_of_fn(
        &mut self,
        function: ObjectId,
        offset: usize,
        superclass: Option<ObjectId>,
    ) {
        let nested: Vec<ObjectId> = match self.heap.get(function) {
            Some(Object::Fn(function)) => function
                .chunk
                .constants
                .iter()
                .filter_map(|constant| constant.as_object())
                .filter(|id| matches!(self.heap.get(*id), Some(Object::Fn(_))))
                .collect(),
            _ => return,
        };
        if let Some(Object::Fn(function)) = self.heap.get_mut(function) {
            function.field_offset = offset;
            function.super_class = superclass;
        }
        for id in nested {
            self.set_field_offset_of_fn(id, offset, superclass);
        }
    }

    fn instantiate(&mut self, class: Value) -> Result<Value, RuntimeError> {
        let Some(class_id) = class.as_object() else {
            return Err(RuntimeError::new("Not a class."));
        };
        let fields = match self.heap.get(class_id) {
            Some(Object::Class(class)) => class.num_fields.max(0) as usize,
            _ => return Err(RuntimeError::new("Not a class.")),
        };
        let id = self.heap.allocate(Object::Instance(ObjInstance {
            class: class_id,
            fields: alloc::vec![Value::NULL; fields],
        }));
        Ok(Value::object(id))
    }
}

impl Default for Vm {
    fn default() -> Vm {
        Vm::new()
    }
}
