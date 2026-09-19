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

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::bytecode::{Chunk, Op};
use crate::compiler;
use crate::core;
use crate::handle::ObjectId;
use crate::heap::Heap;
use crate::math;
use crate::object::{Method, ObjString, Object};
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
            output: Vec::new(),
        };
        core::install(&mut vm);
        vm
    }

    /// Compile `source` and run it.
    pub fn interpret(&mut self, source: &str) -> Result<(), WrenError> {
        let chunk = compiler::compile(self, source)
            .map_err(|error| WrenError::Compile { message: error.message, line: error.line })?;
        self.run(&chunk).map_err(WrenError::Runtime)
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
            Some(Object::Instance(_)) => "<instance>".to_string(),
            None => "<collected>".to_string(),
        }
    }

    /// Run a compiled chunk.
    pub fn run(&mut self, chunk: &Chunk) -> Result<(), RuntimeError> {
        let mut ip = 0usize;

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
                    self.stack.push(self.stack[slot]);
                }
                Op::StoreLocal => {
                    let slot = chunk.code[ip] as usize;
                    ip += 1;
                    // Assignment is an expression in Wren, so the value stays.
                    self.stack[slot] = *self.stack.last().unwrap();
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
                Op::Call => {
                    let arity = chunk.code[ip] as usize;
                    ip += 1;
                    let symbol = chunk.read_short(ip) as usize;
                    ip += 2;

                    let receiver_at = self.stack.len() - arity - 1;
                    let receiver = self.stack[receiver_at];

                    let Some(class) = self.class_of(receiver) else {
                        return Err(RuntimeError {
                            message: "receiver has no class".to_string(),
                            line,
                        });
                    };
                    let Some(method) = self.find_method(class, symbol) else {
                        let name = self.method_names.name(symbol).unwrap_or("?").to_string();
                        let class_name = self.to_string(Value::object(class));
                        return Err(RuntimeError {
                            message: format!("{class_name} does not implement '{name}'."),
                            line,
                        });
                    };

                    let result = match method {
                        Method::Primitive(function) => function(self, receiver_at),
                    };
                    let value = result.map_err(|mut error| {
                        error.line = line;
                        error
                    })?;

                    self.stack.truncate(receiver_at);
                    self.stack.push(value);
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
                    // Leave the value if it short-circuits; it is the result.
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
                Op::Return | Op::End => return Ok(()),
            }
        }
    }
}

impl Default for Vm {
    fn default() -> Vm {
        Vm::new()
    }
}
