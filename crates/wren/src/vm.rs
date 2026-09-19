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
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::bytecode::{Chunk, Op};
use crate::compiler;
use crate::core;
use crate::handle::ObjectId;
use crate::heap::Heap;
use crate::object::{
    Method, ObjClass, ObjClosure, ObjFiber, ObjFn, ObjInstance, ObjString, ObjUpvalue, Object,
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

/// Why the interpreter keeps `chunk`, `ip` and `base` in locals
/// ------------------------------------------------------------
///
/// The obvious shape is to read them out of `self.frames.last()` on every
/// instruction. It is also the shape that makes the fetch -- the single
/// hottest path in the whole VM -- cost a bounds check, a pointer chase and a
/// field load per opcode, before any work is done.
///
/// So the running frame's three fields are hoisted into locals for the
/// duration, and written back only when a call or a return changes which frame
/// is running. The cost of that decision is real and worth naming: **every
/// path that leaves the loop has to remember to park `ip` first**, or the
/// frame resumes at a stale instruction. That is why `park_current` takes `ip`
/// as an argument rather than reading it from anywhere -- there is nowhere to
/// read it from, and making it a parameter turns a silent bug into a
/// compile error.
const _: () = ();

/// One call in progress.
///
/// `Copy`, which is what lets the interpreter take a frame out of the stack to
/// read its fields without borrowing `self` for the rest of the expression.
/// Three words is cheap enough that the alternative -- a borrow that conflicts
/// with the next `self.stack.push` -- is not worth arguing with.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub closure: ObjectId,
    /// Where to resume. Only written when this frame stops being the running
    /// one; while it runs, the interpreter keeps it in a local.
    pub ip: usize,
    /// The stack slot holding the receiver. Local slot *n* is `base + n`, and
    /// slot 0 is `this`.
    pub base: usize,
}

/// A request from a primitive to continue in a different fiber.
pub struct Switch {
    /// Where to go.
    pub target: ObjectId,
    /// The value the target receives: the argument to `call`, or what `yield`
    /// hands back to whoever resumed this fiber.
    pub value: Value,
    /// Whether the target should record this fiber as its caller. `transfer`
    /// does not, which is what makes it a jump rather than a call.
    pub set_caller: bool,
    /// Whether the resumer wants an error handed back rather than propagated.
    pub catching: bool,
    /// Set when the switch is a fiber finishing rather than yielding.
    pub finishing: bool,
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

    /// The value of a variable by name.
    pub fn get(&self, name: &str) -> Option<Value> {
        self.values.get(self.names.find(name)?).copied()
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
    /// Every module that has been loaded, main first.
    ///
    /// **A module is a namespace, not a file.** Two files that import each
    /// other see different sets of names, and a variable defined in one is not
    /// visible in the other unless it is imported -- which is the whole point
    /// of the chapter. One flat namespace would pass most of the suite and get
    /// the interesting half wrong.
    pub modules: Vec<Module>,
    /// Module name to index, so an import can find one already loaded.
    pub module_index: BTreeMap<String, usize>,
    /// How to find a module's source, if the host can.
    ///
    /// `None` means imports fail, which is right for a firmware with no
    /// filesystem. The host sets one; see `Vm::set_module_loader`.
    #[allow(clippy::type_complexity)]
    pub module_loader: Option<Box<dyn Fn(&str) -> Option<String>>>,
    /// How many of the main module's variables are the core library.
    ///
    /// Every module implicitly gets the core classes -- `Num`, `List`,
    /// `System` and the rest -- without importing them, so a new module starts
    /// as a copy of these and nothing else. Recorded once after the core is
    /// installed, because anything defined later is the *program's* and must
    /// not leak into a module that did not ask for it.
    core_variables: usize,

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
    /// The class of a coroutine.
    pub fiber_class: ObjectId,

    /// The fiber currently running. Its stack and frames are the VM's own,
    /// and are swapped back into it when control moves elsewhere.
    pub current_fiber: Option<ObjectId>,
    /// Set by a primitive that wants the interpreter to resume somewhere else.
    ///
    /// A primitive returns a `Value`, which cannot express "do not push a
    /// result, run a different fiber instead". Rather than change every
    /// primitive's signature for the handful that switch, the switching ones
    /// leave the request here and the call site checks for it.
    pub pending_switch: Option<Switch>,
    /// How deep the frames were when Rust last re-entered the interpreter.
    ///
    /// A yield may not cross that boundary: there is a Rust stack frame in the
    /// way that cannot be suspended. `list.each { Fiber.yield }` is the shape
    /// that hits it, and an error is better than corrupting the stack.
    rust_floor: usize,

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
        let fiber_class = class_named(&mut heap, "Fiber", root);

        let mut vm = Vm {
            heap,
            stack: Vec::new(),
            method_names: SymbolTable::new(),
            modules: alloc::vec![Module::new()],
            module_index: BTreeMap::new(),
            module_loader: None,
            core_variables: 0,
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
            fiber_class,
            current_fiber: None,
            pending_switch: None,
            rust_floor: 0,
            frames: Vec::new(),
            open_upvalues: Vec::new(),
            output: Vec::new(),
        };
        core::install(&mut vm);
        vm.core_variables = vm.modules[0].values.len();
        vm
    }

    /// Compile `source` and run it.
    pub fn interpret(&mut self, source: &str) -> Result<(), WrenError> {
        let chunk = compiler::compile(self, source)
            .map_err(|error| WrenError::Compile { message: error.message, line: error.line })?;
        self.run(Rc::new(chunk)).map_err(WrenError::Runtime)
    }

    /// Tell the VM how to find a module's source.
    ///
    /// Without one, `import` fails -- which is the right default for a
    /// firmware with no filesystem to read from.
    pub fn set_module_loader(&mut self, loader: impl Fn(&str) -> Option<String> + 'static) {
        self.module_loader = Some(Box::new(loader));
    }

    /// Find a module by name, loading and running it if this is the first time.
    ///
    /// Returns its index, and whether it had to be run -- the caller needs the
    /// second because running it means continuing in a new frame rather than
    /// falling through.
    fn load_module(&mut self, name: &str) -> Result<(usize, Option<ObjectId>), RuntimeError> {
        if let Some(index) = self.module_index.get(name) {
            return Ok((*index, None));
        }

        let Some(loader) = self.module_loader.as_ref() else {
            return Err(RuntimeError::new(format!("Could not load module '{name}'.")));
        };
        let Some(source) = loader(name) else {
            return Err(RuntimeError::new(format!("Could not load module '{name}'.")));
        };

        // A fresh namespace seeded with the core library.
        let mut module = Module::new();
        for index in 0..self.core_variables {
            let variable = self.modules[0]
                .names
                .name(index)
                .map(ToString::to_string)
                .unwrap_or_default();
            let value = self.modules[0].values[index];
            module.define(&variable, value);
        }
        self.modules.push(module);
        let index = self.modules.len() - 1;
        // Registered *before* compiling, so a module that imports itself finds
        // the partially built one rather than looping forever.
        self.module_index.insert(name.to_string(), index);

        let chunk = compiler::compile_in(self, &source, index).map_err(|error| RuntimeError {
            message: error.message,
            line: error.line,
        })?;

        let function = self.heap.allocate(Object::Fn(Box::new(ObjFn {
            chunk: Rc::new(chunk),
            arity: 0,
            num_upvalues: 0,
            name: name.to_string(),
            field_offset: 0,
            super_class: None,
            module: index,
        })));
        let closure = self
            .heap
            .allocate(Object::Closure(Box::new(ObjClosure { function, upvalues: Vec::new() })));
        Ok((index, Some(closure)))
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
            Object::Fiber(_) => Some(self.fiber_class),
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
            return format_number(number);
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
            Some(Object::Fiber(_)) => "<fiber>".to_string(),
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

    /// The code a closure runs.
    ///
    /// Returns an `Rc` clone rather than a reference, and that is the point of
    /// storing chunks behind an `Rc` at all. A `&Chunk` borrowed out of the
    /// heap would be a live immutable borrow of `self` for as long as the call
    /// runs -- and the very next instruction pushes onto `self.stack`. The
    /// choices were an `Rc` clone once per call, `unsafe` to sever the borrow,
    /// or copying the whole chunk per call. A refcount bump per *call* (not per
    /// instruction) is the cheapest of the three by a wide margin.
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

    /// Which module a closure's code resolves its variables against.
    fn module_of(&self, closure: ObjectId) -> usize {
        self.function_of(closure).map_or(0, |function| function.module)
    }

    /// The module of whatever frame is on top.
    fn current_module(&self) -> usize {
        self.frames
            .last()
            .map_or(0, |frame| self.module_of(frame.closure))
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
    ///
    /// **Getting this list wrong is the classic collector bug**, and it fails
    /// silently: an object missed here is freed while still in use, and what
    /// breaks is whatever happens to reuse the slot, somewhere else entirely.
    /// So the rule is to enumerate every place a `Value` can hide rather than
    /// to reason about which ones "must" already be reachable.
    ///
    /// Note what is *not* here: a Rust local holding a freshly allocated
    /// object. There is no way to enumerate those, which is why `Heap::allocate`
    /// never collects and the check happens between instructions, where the
    /// only live references are the ones below.
    fn roots(&self) -> Vec<Value> {
        let mut roots = self.stack.clone();
        for module in &self.modules {
            roots.extend(module.values.iter().copied());
        }
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
            self.object_class, self.fn_class, self.map_entry_class, self.fiber_class,
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

    /// Make the running fiber's stack and frames its own again.
    fn park_current(&mut self, ip: usize) {
        if let Some(frame) = self.frames.last_mut() {
            frame.ip = ip;
        }
        let Some(id) = self.current_fiber else { return };
        let stack = ::core::mem::take(&mut self.stack);
        let frames = ::core::mem::take(&mut self.frames);
        if let Some(Object::Fiber(fiber)) = self.heap.get_mut(id) {
            fiber.stack = stack;
            fiber.frames = frames;
        }
    }

    /// Take a fiber's stack and frames as the VM's own, and run it.
    fn resume(&mut self, target: ObjectId, value: Value) -> Result<(), RuntimeError> {
        let (mut stack, mut frames, entry, done) = match self.heap.get_mut(target) {
            Some(Object::Fiber(fiber)) => (
                ::core::mem::take(&mut fiber.stack),
                ::core::mem::take(&mut fiber.frames),
                fiber.entry,
                fiber.done,
            ),
            _ => return Err(RuntimeError::new("Not a fiber.")),
        };
        if done {
            return Err(RuntimeError::new("Cannot call a finished fiber."));
        }

        if frames.is_empty() {
            // Starting it: the closure is the receiver, and the value resumed
            // with is its argument when it takes one.
            let Some(entry) = entry else {
                return Err(RuntimeError::new("Fiber has no function."));
            };
            let arity = self.arity_of(entry).unwrap_or(0);
            stack.push(Value::object(entry));
            if arity > 0 {
                stack.push(value);
            }
            frames.push(Frame { closure: entry, ip: 0, base: 0 });
        } else {
            // Resuming it: the value is the result of the `yield` that
            // suspended it, and the call that yielded is waiting for exactly
            // one value at the top of its stack.
            stack.push(value);
        }

        self.stack = stack;
        self.frames = frames;
        self.current_fiber = Some(target);
        Ok(())
    }

    /// Carry out a [`Switch`] requested by a primitive.
    fn perform_switch(&mut self, switch: Switch, ip: usize) -> Result<(), RuntimeError> {
        let from = self.current_fiber;
        self.park_current(ip);

        if switch.set_caller {
            if let Some(Object::Fiber(fiber)) = self.heap.get_mut(switch.target) {
                fiber.caller = from;
                fiber.catching = switch.catching;
            }
        }
        if switch.finishing {
            if let Some(from) = from {
                if let Some(Object::Fiber(fiber)) = self.heap.get_mut(from) {
                    fiber.done = true;
                }
            }
        }
        self.resume(switch.target, switch.value)
    }

    /// The fiber to deliver an error to, if any is willing to catch it.
    fn catcher(&self) -> Option<ObjectId> {
        let mut current = self.current_fiber;
        while let Some(id) = current {
            let Some(Object::Fiber(fiber)) = self.heap.get(id) else { return None };
            if fiber.catching {
                return fiber.caller;
            }
            current = fiber.caller;
        }
        None
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
        let was = self.rust_floor;
        self.rust_floor = depth;
        let chunk = self.push_frame(closure, base)?;
        let result = self.run_frames(chunk, depth);
        self.rust_floor = was;
        result
    }

    /// Run a compiled chunk as a fresh top-level frame.
    pub fn run(&mut self, chunk: Rc<Chunk>) -> Result<(), RuntimeError> {
        self.run_module(chunk, 0)
    }

    /// Run a chunk compiled against a particular module.
    pub fn run_module(&mut self, chunk: Rc<Chunk>, module: usize) -> Result<(), RuntimeError> {
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
            module,
        })));
        let closure = self
            .heap
            .allocate(Object::Closure(Box::new(ObjClosure { function, upvalues: Vec::new() })));

        // The module runs in a root fiber, so that `Fiber.yield` at the top
        // level has something to complain about and `Fiber.current` has an
        // answer.
        if self.current_fiber.is_none() {
            let root = self.heap.allocate(Object::Fiber(Box::new(ObjFiber::new(closure))));
            self.current_fiber = Some(root);
        }

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
        // Cached with `ip` and `base` for the same reason: module variable
        // access would otherwise reach through the frame to the closure to the
        // function on every load.
        let mut module = self
            .frames
            .last()
            .and_then(|frame| self.function_of(frame.closure))
            .map_or(0, |function| function.module);

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
                    self.stack.push(self.modules[module].values[index]);
                }
                Op::StoreModuleVar => {
                    let index = chunk.read_short(ip) as usize;
                    ip += 2;
                    self.modules[module].values[index] = *self.stack.last().unwrap();
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
                    let found = self.find_method(start_from, symbol);
                    let Some(method) = found else {
                        let name = self.method_names.name(symbol).unwrap_or("?").to_string();
                        let receiver = self.stack[receiver_at];
                        let class_name = self
                            .class_of(receiver)
                            .map(|class| self.class_name(class))
                            .unwrap_or_else(|| "null".to_string());
                        let error = RuntimeError {
                            message: format!("{class_name} does not implement '{name}'."),
                            line,
                        };
                        match self.deliver_error(error, ip)? {
                            Some((next_chunk, next_ip, next_base)) => {
                                chunk = next_chunk;
                                ip = next_ip;
                                base = next_base;
                                module = self.current_module();
                                continue;
                            }
                            None => unreachable!("deliver_error returns or switches"),
                        }
                    };

                    match method {
                        Method::Primitive(function) => {
                            let outcome = function(self, receiver_at).map_err(|mut error| {
                                if error.line == 0 {
                                    error.line = line;
                                }
                                error
                            });

                            let value = match outcome {
                                Ok(value) => value,
                                Err(error) => match self.deliver_error(error, ip)? {
                                    Some((next_chunk, next_ip, next_base)) => {
                                        chunk = next_chunk;
                                        ip = next_ip;
                                        base = next_base;
                                        module = self.current_module();
                                        continue;
                                    }
                                    None => unreachable!("deliver_error returns or switches"),
                                },
                            };

                            // **A primitive may have asked to continue
                            // somewhere else.** The result slot is cleared
                            // either way; a switch leaves the target to push
                            // its own value there when it comes back.
                            self.stack.truncate(receiver_at);
                            if let Some(switch) = self.pending_switch.take() {
                                self.perform_switch(switch, ip)?;
                                let frame = *self.frames.last().expect("a frame to resume");
                                ip = frame.ip;
                                base = frame.base;
                                chunk = self.chunk_of(frame.closure)?;
                                module = self.module_of(frame.closure);
                                continue;
                            }
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
                            module = self.module_of(closure);
                        }
                    }
                }
                Op::ImportModule => {
                    let index = chunk.read_short(ip) as usize;
                    ip += 2;
                    let name = self.to_string(chunk.constants[index]);

                    match self.load_module(&name) {
                        Ok((_, None)) => {
                            // Already loaded: its body must not run twice.
                            self.stack.push(Value::NULL);
                        }
                        Ok((_, Some(closure))) => {
                            // Run the module body as an ordinary call. Its
                            // return value lands where this instruction's
                            // result would have, so nothing special is needed
                            // on the way back.
                            if let Some(frame) = self.frames.last_mut() {
                                frame.ip = ip;
                            }
                            let base = self.stack.len();
                            self.stack.push(Value::NULL);
                            chunk = self.push_frame(closure, base)?;
                            ip = 0;
                            module = self.module_of(closure);
                            continue;
                        }
                        Err(error) => match self.deliver_error(error, ip)? {
                            Some((next_chunk, next_ip, next_base)) => {
                                chunk = next_chunk;
                                ip = next_ip;
                                base = next_base;
                                module = self.current_module();
                                continue;
                            }
                            None => unreachable!("deliver_error returns or switches"),
                        },
                    }
                }
                Op::ImportVariable => {
                    let module_name = chunk.read_short(ip) as usize;
                    let variable_name = chunk.read_short(ip + 2) as usize;
                    ip += 4;

                    let module_name = self.to_string(chunk.constants[module_name]);
                    let variable = self.to_string(chunk.constants[variable_name]);

                    let Some(from) = self.module_index.get(&module_name).copied() else {
                        return Err(RuntimeError {
                            message: format!("Could not load module '{module_name}'."),
                            line,
                        });
                    };
                    let Some(value) = self.modules[from].get(&variable) else {
                        return Err(RuntimeError {
                            message: format!(
                                "Could not find a variable named '{variable}' in module '{module_name}'."
                            ),
                            line,
                        });
                    };
                    self.stack.push(value);
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
                        // **A fiber running out of frames is finished**, and
                        // control goes back to whoever resumed it rather than
                        // out of the interpreter -- unless nobody did, in which
                        // case this is the root and the program is over.
                        let caller = self.current_fiber.and_then(|id| match self.heap.get(id) {
                            Some(Object::Fiber(fiber)) => fiber.caller,
                            _ => None,
                        });
                        if let (Some(caller), true) = (caller, floor <= self.rust_floor) {
                            self.perform_switch(
                                Switch {
                                    target: caller,
                                    value: result,
                                    set_caller: false,
                                    catching: false,
                                    finishing: true,
                                },
                                ip,
                            )?;
                            let frame = *self.frames.last().expect("a frame to resume");
                            ip = frame.ip;
                            base = frame.base;
                            chunk = self.chunk_of(frame.closure)?;
                            module = self.module_of(frame.closure);
                            continue;
                        }
                        if let Some(id) = self.current_fiber {
                            if let Some(Object::Fiber(fiber)) = self.heap.get_mut(id) {
                                fiber.done = true;
                            }
                        }
                        return Ok(result);
                    }

                    self.stack.push(result);
                    let frame = *self.frames.last().unwrap();
                    ip = frame.ip;
                    base = frame.base;
                    chunk = self.chunk_of(frame.closure)?;
                    module = self.module_of(frame.closure);
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

            // **Between instructions, and only here.** Upstream collects inside
            // the allocator, which means any allocation can free an object the
            // caller is half way through building and holding only in a C
            // local; upstream handles that with a stack of temporary roots the
            // caller must remember to push, and forgetting one is a classic
            // source of collector bugs.
            //
            // Checking here instead costs a branch per instruction and removes
            // the whole category: at an instruction boundary the live set is
            // exactly what `roots` enumerates, with nothing in flight.
            if self.heap.should_collect() {
                if let Some(frame) = self.frames.last_mut() {
                    frame.ip = ip;
                }
                let roots = self.roots();
                self.heap.collect(roots);
            }
        }
    }

    /// Hand a runtime error to the nearest fiber that asked to catch one.
    ///
    /// Returns where to resume, or propagates the error when nothing is
    /// catching -- which is what makes an uncaught error end the program.
    #[allow(clippy::type_complexity)]
    fn deliver_error(
        &mut self,
        error: RuntimeError,
        ip: usize,
    ) -> Result<Option<(Rc<Chunk>, usize, usize)>, RuntimeError> {
        let Some(catcher) = self.catcher() else {
            return Err(error);
        };

        let message = self.new_string(&error.message);
        if let Some(id) = self.current_fiber {
            if let Some(Object::Fiber(fiber)) = self.heap.get_mut(id) {
                fiber.error = message;
                fiber.done = true;
            }
        }

        self.perform_switch(
            Switch {
                target: catcher,
                value: message,
                set_caller: false,
                catching: false,
                finishing: false,
            },
            ip,
        )?;
        let frame = *self.frames.last().expect("a frame to resume");
        let chunk = self.chunk_of(frame.closure)?;
        Ok(Some((chunk, frame.ip, frame.base)))
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

/// Format a number the way Wren does.
///
/// **This is `printf("%.14g")` plus three special cases**, and matching it
/// matters more than it looks: number formatting is in the output of a large
/// share of the test suite, so a formatter that is merely reasonable fails
/// dozens of tests that are not about formatting at all.
///
/// Why fourteen significant digits rather than seventeen, which is what it
/// takes to round-trip an `f64`? Because the point is readability, not
/// round-tripping. `0.1 + 0.2` is `0.30000000000000004` at full precision and
/// `0.3` at fourteen digits, and the second is what a person writing a script
/// means. The cost is that two distinct doubles can print identically; Wren
/// accepts that trade and so must anything compatible with it.
///
/// `%g` itself is the rule that picks between decimal and exponential: use
/// exponential when the exponent is below -4 or at least the precision, and
/// decimal otherwise, then strip trailing zeros either way. That is why `1e300`
/// prints as `1e+300` while `1000` prints as `1000`.
fn format_number(value: f64) -> String {
    // Wren spells these out rather than using C's "inf"/"-inf"/"nan", so a
    // program's output is the same on every platform -- C leaves the spelling
    // implementation-defined, which is exactly the sort of thing that makes a
    // test suite portable or not.
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "infinity" } else { "-infinity" }.to_string();
    }
    if value == 0.0 {
        // `-0.0` is a real value a program can produce -- `0 * -1`, or
        // `(-0.5).truncate` -- and Wren prints its sign.
        return if value.is_sign_negative() { "-0" } else { "0" }.to_string();
    }

    // Rust has no `%g`, so it is built from `%e`: format with 13 digits after
    // the point (14 significant), then read back the exponent to decide which
    // shape to print. Doing it in this order means the rounding happens once,
    // before the decision, which is what C does.
    let exponential = format!("{value:.13e}");
    let (mantissa, exponent) = exponential
        .split_once('e')
        .expect("Rust's {:e} always writes an exponent");
    let exponent: i32 = exponent.parse().expect("and it is always an integer");

    // Written as two comparisons rather than a range check because this *is*
    // `%g`'s rule as the C standard states it -- "style e is used if the
    // exponent is less than -4 or greater than or equal to the precision" --
    // and a reader checking this against the standard should see the same
    // shape. `(-4..14).contains()` would be the same test and a worse mirror.
    #[allow(clippy::manual_range_contains)]
    if exponent < -4 || exponent >= 14 {
        let mantissa = trim_trailing_zeros(mantissa);
        let sign = if exponent < 0 { '-' } else { '+' };
        // C pads the exponent to at least two digits: `1e+05`, not `1e+5`.
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }

    // Decimal: 14 significant digits means 13 - exponent after the point.
    let decimals = (13 - exponent).max(0) as usize;
    trim_trailing_zeros(&format!("{value:.decimals$}"))
}

/// Strip the trailing zeros `%g` removes, and the point if nothing follows it.
fn trim_trailing_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}
