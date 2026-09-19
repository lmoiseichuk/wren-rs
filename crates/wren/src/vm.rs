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
#[cfg(feature = "compiler")]
use crate::compiler;
use crate::core;
use crate::handle::ObjectId;
use crate::heap::Heap;
use crate::object::{
    Method, ObjClass, ObjClosure, ObjFiber, ObjFn, ObjString, ObjUpvalue, Object, ObjectType,
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
        RuntimeError {
            message: message.into(),
            line: 0,
        }
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

/// How many objects the nursery holds before it is collected.
///
/// **This is the bound on floating garbage**, which is the reason to have a
/// nursery on a fixed heap: peak memory is the live set plus at most this many
/// young objects. Too small and the roots are scanned constantly; too large
/// and the bound stops being useful.
const NURSERY_OBJECTS: usize = 2048;

/// Whether this build has a young generation.
///
/// A constant so that the check above folds away entirely when it does not:
/// `heap.young()` is a field read, and a field read and a comparison on every
/// instruction is not free even when the answer is always no.
const NURSERY: bool = cfg!(feature = "nursery");

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
/// **The frame carries its own code and module**, which is the difference
/// between a return costing a field read and a return costing four heap
/// lookups. Resuming a caller needs its chunk and the module its variables
/// resolve against; both are reachable from `closure`, but only by going
/// closure -> function -> chunk through the heap table, twice, on every single
/// return. Storing them costs two words per frame and a refcount bump per
/// call, and removes that walk from the hot path entirely.
///
/// It used to be `Copy`, so the interpreter could lift a frame out of the
/// stack without borrowing `self`. An `Rc` makes that impossible, so the
/// handful of sites that did it read the fields they need through a borrow
/// that ends in the same statement instead.
#[derive(Debug, Clone)]
pub struct Frame {
    pub closure: ObjectId,
    /// Where to resume. Only written when this frame stops being the running
    /// one; while it runs, the interpreter keeps it in a local.
    pub ip: usize,
    /// The stack slot holding the receiver. Local slot *n* is `base + n`, and
    /// slot 0 is `this`.
    pub base: usize,
    /// The code this frame runs, so resuming it does not have to find it again.
    pub chunk: Rc<Chunk>,
    /// The module this frame's code resolves its variables against.
    pub module: usize,
    /// Where this method's own fields start in its receiver.
    ///
    /// **Read on every field access**, which is why it is here rather than
    /// fetched. A method defined on a subclass sees its fields after the ones
    /// its superclass declared, so the offset is a property of the function
    /// and fixed for as long as the frame runs. Finding it used to mean
    /// walking closure -> function through the heap for each `x` and each
    /// `x = ...`; a `u16` on the frame makes it a field read. Wren caps a
    /// class at 255 fields, so the width is not a limit anyone can reach.
    pub field_offset: u16,
}

/// What entering a call needs from the closure it is entering.
///
/// See [`Vm::call_target`], which fills it in with a single walk of the heap.
struct CallTarget {
    chunk: Rc<Chunk>,
    arity: usize,
    module: usize,
    field_offset: u16,
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
    /// Deliver `value` to the target as an *error* rather than as a result.
    /// This is what `transferError` does: hand control over and fail there.
    pub as_error: bool,
}

/// A module's variables: names and values, in parallel.
pub struct Module {
    /// The resolved name this module was loaded under, empty for the main one.
    ///
    /// **Kept because a relative import resolves against its importer**, not
    /// against the program's entry point: `sub/module.wren` saying
    /// `import "./module_2"` means `sub/module_2`. Without the importer's own
    /// name there is nothing to resolve against.
    pub name: String,
    pub names: SymbolTable,
    pub values: Vec<Value>,
}

impl Default for Module {
    fn default() -> Module {
        Module::new()
    }
}

impl Module {
    pub fn new() -> Module {
        Module {
            name: String::new(),
            names: SymbolTable::new(),
            values: Vec::new(),
        }
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
    /// Every Rust-implemented method, indexed by the number a packed method
    /// table entry carries.
    ///
    /// **A function pointer does not fit in a packed entry beside its tag**,
    /// so the table holds the pointers and the entry holds an index. The
    /// indirection is one load from a small, hot vector and it falls only on
    /// primitive calls; a closure decodes straight to its handle.
    pub primitives: Vec<crate::object::Primitive>,
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
    /// What `System.clock` reads: seconds, monotonic, origin unspecified.
    ///
    /// **A host hook rather than a call into `std`.** A firmware has no clock
    /// this crate could know about -- on an ESP32 it is `esp_timer_get_time`,
    /// on a CH32 a timer peripheral -- and a benchmark that cannot time itself
    /// is not much of a benchmark. With `std` it defaults to the process
    /// clock, so the host needs no setup.
    #[allow(clippy::type_complexity)]
    pub clock: Option<Box<dyn Fn() -> f64>>,
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
    /// `Random`, once the built-in module has been imported. Until then this
    /// points at `Object` and nothing refers to it.
    pub random_class: ObjectId,
    /// The base class of everything iterable.
    ///
    /// `List`, `Map`, `Range` and `String` all inherit from it, and so can a
    /// class written in Wren -- which is the point: answering `iterate(_)` and
    /// `iteratorValue(_)` is the whole contract, and everything else comes
    /// from here.
    pub sequence_class: ObjectId,
    /// The lazy views `map`, `where`, `take` and `skip` return.
    pub map_sequence_class: ObjectId,
    pub where_sequence_class: ObjectId,
    pub take_sequence_class: ObjectId,
    pub skip_sequence_class: ObjectId,
    /// What `map.keys` and `map.values` return: views over the same table,
    /// yielding one half of each entry.
    pub map_key_sequence_class: ObjectId,
    pub map_value_sequence_class: ObjectId,
    /// What `Class.attributes` returns: a `self` map and a `methods` map.
    pub class_attributes_class: ObjectId,
    /// What `String.bytes` and `String.codePoints` return: views over the
    /// string, not copies of it.
    pub string_byte_sequence_class: ObjectId,
    pub string_code_point_sequence_class: ObjectId,

    /// The fiber currently running. Its stack and frames are the VM's own,
    /// and are swapped back into it when control moves elsewhere.
    pub current_fiber: Option<ObjectId>,
    /// The fiber the program started in. Calling it is an error: it is the one
    /// doing the calling, so resuming it would re-enter a live stack.
    pub root_fiber: Option<ObjectId>,
    /// Set by a primitive that wants the interpreter to resume somewhere else.
    ///
    /// A primitive returns a `Value`, which cannot express "do not push a
    /// result, run a different fiber instead". Rather than change every
    /// primitive's signature for the handful that switch, the switching ones
    /// leave the request here and the call site checks for it.
    pub pending_switch: Option<Switch>,
    /// Set when the program should stop, short of an error.
    ///
    /// Only `Fiber.yield` from the root fiber sets it: there is nobody to hand
    /// control back to, and upstream ends the run rather than failing.
    pub halting: bool,
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
    /// A reusable buffer for the handles a candidate flush checks against.
    root_handles: Vec<ObjectId>,

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
        // Reparented below, once `Sequence` exists -- the classes have to be
        // created before it because `class_named` needs somewhere to put the
        // name string first.
        let class_class = class_named(&mut heap, "Class", root);
        let fn_class = class_named(&mut heap, "Fn", root);
        let map_entry_class = class_named(&mut heap, "MapEntry", root);
        let fiber_class = class_named(&mut heap, "Fiber", root);
        let sequence_class = class_named(&mut heap, "Sequence", root);
        let iterable = Some(sequence_class);
        let map_sequence_class = class_named(&mut heap, "MapSequence", iterable);
        let where_sequence_class = class_named(&mut heap, "WhereSequence", iterable);
        let take_sequence_class = class_named(&mut heap, "TakeSequence", iterable);
        let skip_sequence_class = class_named(&mut heap, "SkipSequence", iterable);
        let map_key_sequence_class = class_named(&mut heap, "MapKeySequence", iterable);
        let map_value_sequence_class = class_named(&mut heap, "MapValueSequence", iterable);
        let class_attributes_class = class_named(&mut heap, "ClassAttributes", root);
        let string_byte_sequence_class = class_named(&mut heap, "StringByteSequence", iterable);
        let string_code_point_sequence_class =
            class_named(&mut heap, "StringCodePointSequence", iterable);

        let mut vm = Vm {
            heap,
            stack: Vec::new(),
            method_names: SymbolTable::new(),
            primitives: Vec::new(),
            modules: alloc::vec![Module::new()],
            module_index: BTreeMap::new(),
            module_loader: None,
            clock: None,
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
            random_class: object_class,
            sequence_class,
            map_sequence_class,
            where_sequence_class,
            take_sequence_class,
            skip_sequence_class,
            map_key_sequence_class,
            map_value_sequence_class,
            class_attributes_class,
            string_byte_sequence_class,
            string_code_point_sequence_class,
            current_fiber: None,
            root_fiber: None,
            pending_switch: None,
            halting: false,
            rust_floor: 0,
            frames: Vec::new(),
            open_upvalues: Vec::new(),
            root_handles: Vec::new(),
            output: Vec::new(),
        };
        // `List`, `Map`, `Range` and `String` are sequences.
        for class in [list_class, map_class, range_class, string_class] {
            if let Some(class) = vm.heap.class_mut(class) {
                class.superclass = Some(sequence_class);
            }
        }

        core::install(&mut vm);
        // **Only now.** The classes above were created empty and populated by
        // `install`, so flattening any earlier would have copied nothing.
        vm.flatten_class_hierarchy();
        vm.core_variables = vm.modules[0].values.len();

        // On a host there is an obvious clock and no reason to make every
        // caller wire one up.
        #[cfg(feature = "std")]
        {
            let origin = std::time::Instant::now();
            vm.set_clock(move || origin.elapsed().as_secs_f64());
        }

        vm
    }

    /// Compile `source` and run it.
    ///
    /// **Absent without the `compiler` feature**, which is the point of that
    /// feature: a device running `.wrenc` has no use for a lexer and a parser,
    /// and leaving them out is most of what makes the image fit. Use
    /// [`Vm::run_closure`] with a loaded closure instead.
    #[cfg(feature = "compiler")]
    pub fn interpret(&mut self, source: &str) -> Result<(), WrenError> {
        let chunk = compiler::compile(self, source).map_err(|error| WrenError::Compile {
            message: error.message,
            line: error.line,
        })?;
        self.run(Rc::new(chunk)).map_err(WrenError::Runtime)
    }

    /// Collect now, from whatever the current roots are.
    ///
    /// Exposed because `System.gc()` exists and because a test that wants to
    /// prove something about the collector needs to be able to provoke it
    /// rather than allocate until one happens by luck.
    pub fn collect_garbage(&mut self) {
        let roots = self.roots();
        self.heap.collect(roots);
    }

    /// Name the module a program itself is compiled into.
    ///
    /// **`Meta.getModuleVariables` can be asked about the main module**, which
    /// means it needs a name to be found under. A host that runs a file names
    /// it after the file; one evaluating a string may leave it unnamed.
    pub fn set_main_module_name(&mut self, name: &str) {
        self.modules[0].name = name.to_string();
        self.module_index.insert(name.to_string(), 0);
    }

    /// Tell the VM how to read a clock, for `System.clock`.
    pub fn set_clock(&mut self, clock: impl Fn() -> f64 + 'static) {
        self.clock = Some(Box::new(clock));
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

        // Built-in modules are constructed rather than loaded: there is no
        // source to find, and a firmware with no loader at all should still be
        // able to `import "random"`.
        if name == "random" {
            let index = core::install_random(self);
            // `random` brings a class and a metaclass with it, neither of
            // which existed when the hierarchy was last flattened.
            self.flatten_class_hierarchy();
            return Ok((index, None));
        }
        if name == "meta" {
            let index = core::install_meta(self);
            self.flatten_class_hierarchy();
            return Ok((index, None));
        }

        let Some(loader) = self.module_loader.as_ref() else {
            return Err(RuntimeError::new(format!(
                "Could not load module '{name}'."
            )));
        };
        let Some(source) = loader(name) else {
            return Err(RuntimeError::new(format!(
                "Could not load module '{name}'."
            )));
        };

        // A fresh namespace seeded with the core library.
        let mut module = Module::new();
        module.name = name.to_string();
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

        // **A module can only be loaded where there is a compiler.** A build
        // without one runs bytecode it was handed, and `import` of a source
        // file is not a thing it can do -- saying so is better than a loader
        // that silently finds nothing.
        #[cfg(not(feature = "compiler"))]
        {
            let _ = source;
            return Err(RuntimeError::new(alloc::format!(
                "Cannot import '{name}': this build has no compiler."
            )));
        }

        #[cfg(feature = "compiler")]
        {
            let chunk =
                compiler::compile_in(self, &source, index).map_err(|error| RuntimeError {
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
                owner_class: None,
                module: index,
            })));
            let closure = self.heap.allocate(Object::Closure(ObjClosure {
                function,
                upvalues: Vec::new(),
            }));
            Ok((index, Some(closure)))
        }
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
        // **Seven of these ten answers do not need the object at all.** A
        // `List` is a `List` whatever is in it, so once the type is in the
        // handle this whole path is a shift and a table lookup -- and this is
        // the dispatch path, reached on every method call.
        match self.heap.kind_of(id)? {
            ObjectType::String => Some(self.string_class),
            ObjectType::List => Some(self.list_class),
            ObjectType::Map => Some(self.map_class),
            ObjectType::Range => Some(self.range_class),
            ObjectType::Fn | ObjectType::Closure => Some(self.fn_class),
            ObjectType::Fiber => Some(self.fiber_class),
            // A class's own class is its metaclass, which is what makes a
            // static call like `System.print` land on the right table.
            ObjectType::Class => Some(self.heap.class(id)?.metaclass.unwrap_or(self.class_class)),
            ObjectType::Instance => Some(self.heap.instance(id)?.class),
            // An upvalue is never a value a program can hold; it exists only
            // inside a closure.
            ObjectType::Upvalue => None,
        }
    }

    /// Find a method. **One array index, no chain walk.**
    ///
    /// Every class's table already holds its inherited methods, copied down
    /// when the class was created -- see [`Vm::inherit_methods`]. So this is a
    /// bounds check and a load, whatever the depth of the hierarchy, which is
    /// what upstream does and why its dispatch is quick.
    ///
    /// The chain is still walked for `is` and for `super`, which are about the
    /// hierarchy rather than about finding a method in it.
    fn find_method(&self, class: ObjectId, symbol: usize) -> Option<Method> {
        let class = self.heap.class(class)?;
        let entry = class.method_entry(symbol);
        if let Some(closure) = crate::object::entry_closure(entry) {
            return Some(Method::Closure(closure));
        }
        let index = crate::object::entry_primitive(entry)?;
        let function = self.primitives.get(index)?;
        Some(Method::Primitive(*function))
    }

    /// Bind a Rust-implemented method, interning the function pointer.
    pub fn bind_primitive(
        &mut self,
        class: ObjectId,
        symbol: usize,
        function: crate::object::Primitive,
    ) {
        let index = self.primitives.len();
        self.primitives.push(function);
        if let Some(class) = self.heap.class_mut(class) {
            class.define(symbol, crate::object::primitive_entry(index));
        }
    }

    /// Copy `parent`'s methods into `child`, for the ones `child` has not
    /// defined itself.
    ///
    /// **This is what makes dispatch a single index**, and it is upstream's
    /// `bindSuperclass` under another name. It works because a Wren class is
    /// closed: methods are bound when the class body runs and nothing can add
    /// one afterwards, so a copy taken at creation cannot go stale. A language
    /// that allowed reopening a class would have to invalidate these instead,
    /// and would probably be better off walking the chain.
    ///
    /// The `is_none` guard is what makes an override win. It matters for the
    /// core classes, which are populated before they are flattened and so
    /// already hold their own definitions of things like `toString`.
    fn inherit_methods(&mut self, child_id: ObjectId, parent: ObjectId) {
        let inherited = match self.heap.class(parent) {
            Some(parent) => parent.methods.clone(),
            _ => return,
        };
        let Some(child) = self.heap.class_mut(child_id) else {
            return;
        };
        if child.methods.len() < inherited.len() {
            child
                .methods
                .resize(inherited.len(), crate::object::NO_METHOD);
        }
        let mut copied: Vec<ObjectId> = Vec::new();
        for (symbol, entry) in inherited.iter().enumerate() {
            if child.methods[symbol] == crate::object::NO_METHOD {
                child.methods[symbol] = *entry;
                // Copying an entry down makes the child refer to the parent's
                // closure as well, which the barrier has to hear about.
                if let Some(closure) = crate::object::entry_closure(*entry) {
                    copied.push(closure);
                }
            }
        }
        for closure in copied {
            self.heap.wrote(child_id, Value::object(closure));
        }
    }

    /// Flatten every class in the heap, parents before children.
    ///
    /// Run once after the core library is installed, and again after a lazily
    /// built module -- `random`, `meta` -- adds classes. Classes made by
    /// running Wren code do not need it: [`Vm::make_class`] copies from the
    /// superclass at creation, by which time the superclass is already flat.
    ///
    /// Sorting by depth is what lets one pass do it. A child flattened after
    /// its parent inherits the parent's *complete* table, ancestors included,
    /// so nothing has to be visited twice.
    fn flatten_class_hierarchy(&mut self) {
        let mut classes: Vec<(usize, ObjectId)> = Vec::new();
        for id in self.heap.ids() {
            if !self.heap.class(id).is_some() {
                continue;
            }
            // Depth is counted with a step limit rather than trusted: a cycle
            // here would hang the VM at start-up, which is the worst place to
            // find out that a superclass was wired up wrongly.
            let mut depth = 0;
            let mut current = match self.heap.class(id) {
                Some(class) => class.superclass,
                _ => None,
            };
            while let Some(parent) = current {
                depth += 1;
                if depth > 64 {
                    break;
                }
                current = match self.heap.class(parent) {
                    Some(parent) => parent.superclass,
                    _ => None,
                };
            }
            classes.push((depth, id));
        }
        classes.sort_by_key(|(depth, _)| *depth);

        for (_, id) in &classes {
            let parent = match self.heap.class(*id) {
                Some(class) => class.superclass,
                _ => None,
            };
            if let Some(parent) = parent {
                self.inherit_methods(*id, parent);
            }
        }

        // **Hand back the slack now, not at the first collection.** These
        // tables were built by repeated `resize` and each one is holding
        // roughly half as much again as it uses. The collector shrinks class
        // tables too, but a VM that never collects would carry the waste for
        // its whole life -- and `Vm::new`'s resident figure is a published
        // number, so it should be the honest one.
        for (_, id) in &classes {
            if let Some(class) = self.heap.class_mut(*id) {
                class.methods.shrink_to_fit();
            }
        }
    }

    /// Allocate a string and return a value referring to it.
    pub fn new_string(&mut self, text: &str) -> Value {
        Value::object(
            self.heap
                .allocate(Object::String(ObjString::from_text(text))),
        )
    }

    /// Build the `ClassAttributes` pair a class's attributes come back as.
    pub fn new_class_attributes(&mut self, own: Value, methods: Value) -> Value {
        let class = self.class_attributes_class;
        Value::object(self.heap.new_instance(class, &[own, methods]))
    }

    /// Allocate a string from raw bytes.
    ///
    /// **Wren strings are bytes**, and a literal containing `\xff` is not
    /// valid UTF-8. Going through `&str` would either reject it or silently
    /// re-encode it as two bytes; neither is what the program wrote.
    pub fn new_string_bytes(&mut self, bytes: Vec<u8>) -> Value {
        Value::object(self.heap.allocate(Object::String(ObjString::new(bytes))))
    }

    /// Is this value a string?
    ///
    /// **Separate from reading one**, because a Wren string may hold bytes
    /// that are not valid UTF-8 and is a string all the same. Using "can I
    /// read it as `&str`" as the type test made `"\xff"` report as a
    /// non-string, which printed it as `[invalid toString]` and rejected it
    /// wherever a string argument was required.
    pub fn is_string(&self, value: Value) -> bool {
        value
            .as_object()
            .and_then(|id| self.heap.string(id))
            .is_some()
    }

    /// Read a string object, for a primitive that needs its contents.
    pub fn string_at(&self, value: Value) -> Option<&str> {
        match self.heap.string(value.as_object()?) {
            Some(text) => text.as_str(),
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
        // **Dispatch on the handle's type, then fetch what that type needs.**
        // A handle whose type says one thing and whose table has nothing under
        // it cannot happen, but saying so with `unwrap` would be a panic in a
        // printing routine, so each arm falls back to the placeholder it would
        // have printed for a collected object.
        match self.heap.type_of(id) {
            // Lossy, because this is for display: the bytes are kept intact
            // in the string itself, and printing is the one place where a
            // sequence that is not valid UTF-8 has to become *something*.
            Some(ObjectType::String) => match self.heap.string(id) {
                Some(text) => String::from_utf8_lossy(&text.bytes).into_owned(),
                None => "<collected>".to_string(),
            },
            Some(ObjectType::Range) => match self.heap.range(id).copied() {
                Some(range) => {
                    let separator = if range.is_inclusive { ".." } else { "..." };
                    format!(
                        "{}{}{}",
                        self.to_string(Value::num(range.from)),
                        separator,
                        self.to_string(Value::num(range.to))
                    )
                }
                None => "<collected>".to_string(),
            },
            Some(ObjectType::List) => {
                let Some(elements) = self.heap.list(id).map(|list| list.elements.clone()) else {
                    return "<collected>".to_string();
                };
                let mut out = String::from("[");
                for (at, element) in elements.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&self.to_string(*element));
                }
                out.push(']');
                out
            }
            // The dispatching version lives on `Map.toString`; this is the
            // fallback for a value printed without going through a method, and
            // it uses the same shape so the two cannot look different.
            Some(ObjectType::Map) => {
                let Some(entries) = self.heap.map(id).map(|map| map.entries.clone()) else {
                    return "<collected>".to_string();
                };
                let mut out = String::from("{");
                for (index, entry) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&self.to_string(entry.key));
                    out.push_str(": ");
                    out.push_str(&self.to_string(entry.value));
                }
                out.push('}');
                out
            }
            Some(ObjectType::Class) => {
                let name = self.heap.class(id).map(|class| class.name);
                match name.and_then(|name| self.heap.string(name)) {
                    Some(name) => name.as_str().unwrap_or("<class>").to_string(),
                    None => "<class>".to_string(),
                }
            }
            Some(ObjectType::Instance) => match self.heap.instance(id).map(|it| it.class) {
                Some(class) => format!("instance of {}", self.class_name(class)),
                None => "<collected>".to_string(),
            },
            Some(ObjectType::Fn | ObjectType::Closure) => "<fn>".to_string(),
            Some(ObjectType::Fiber) => "<fiber>".to_string(),
            Some(ObjectType::Upvalue) => "<upvalue>".to_string(),
            None => "<collected>".to_string(),
        }
    }

    /// Push a frame for a closure whose receiver and arguments are already on
    /// the stack, with the receiver at `base`.
    fn push_frame(&mut self, closure: ObjectId, base: usize) -> Result<Rc<Chunk>, RuntimeError> {
        if self.frames.len() >= MAX_FRAMES {
            return Err(RuntimeError::new("Stack overflow."));
        }
        let target = self.call_target(closure)?;
        self.frames.push(Frame {
            closure,
            ip: 0,
            base,
            chunk: target.chunk.clone(),
            module: target.module,
            field_offset: target.field_offset,
        });
        Ok(target.chunk)
    }

    /// Everything entering a call needs from a closure, found in **one** walk.
    ///
    /// This is the fix for the indirection the design note complained about.
    /// Making the call used to ask the heap for the same two objects three
    /// times over: `function_of` for the arity, `chunk_of` for the code, and
    /// `module_of` for the namespace -- six table lookups, each a bounds check
    /// and an enum match, to read three fields that sit beside each other in
    /// the same `ObjFn`. Now it is two lookups and three field reads.
    ///
    /// It is a struct rather than a tuple because the caller assigns the
    /// fields into the interpreter's hoisted locals, and `target.module` is
    /// much harder to get wrong there than `.2`.
    ///
    /// **Why the chunk is an `Rc` clone and not a reference.** A `&Chunk`
    /// borrowed out of the heap would be a live immutable borrow of `self` for
    /// as long as the call runs -- and the very next instruction pushes onto
    /// `self.stack`. The choices were an `Rc` clone once per call, `unsafe` to
    /// sever the borrow, or copying the whole chunk per call. A refcount bump
    /// per *call* -- not per instruction -- is the cheapest of the three by a
    /// wide margin.
    fn call_target(&self, closure: ObjectId) -> Result<CallTarget, RuntimeError> {
        let Some(closure) = self.heap.closure(closure) else {
            return Err(RuntimeError::new("Not a closure."));
        };
        let Some(function) = self.heap.function(closure.function) else {
            return Err(RuntimeError::new("Closure has no function."));
        };
        Ok(CallTarget {
            chunk: function.chunk.clone(),
            arity: function.arity,
            module: function.module,
            field_offset: function.field_offset as u16,
        })
    }

    /// How many arguments a closure takes.
    pub fn arity_of(&self, closure: ObjectId) -> Option<usize> {
        self.function_of(closure).map(|function| function.arity)
    }

    /// Which module a closure's code resolves its variables against.
    fn module_of(&self, closure: ObjectId) -> usize {
        self.function_of(closure)
            .map_or(0, |function| function.module)
    }

    /// The module of whatever frame is on top.
    pub(crate) fn current_module(&self) -> usize {
        self.frames
            .last()
            .map_or(0, |frame| self.module_of(frame.closure))
    }

    fn function_of(&self, closure: ObjectId) -> Option<&ObjFn> {
        let closure = self.heap.closure(closure)?;
        match self.heap.function(closure.function) {
            Some(function) => Some(function),
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
            if let Some(upvalue) = self.heap.upvalue(*existing) {
                if upvalue.is_open() && upvalue.slot() == slot {
                    return *existing;
                }
            }
        }
        let id = self
            .heap
            .allocate(Object::Upvalue(ObjUpvalue::new(slot, Value::UNDEFINED)));
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
            let slot = match self.heap.upvalue(id) {
                Some(upvalue) if upvalue.is_open() => upvalue.slot(),
                _ => continue,
            };
            if slot < from {
                still_open.push(id);
                continue;
            }
            let value = self.stack.get(slot).copied().unwrap_or(Value::NULL);
            // **Closing an upvalue turns a stack reference into a heap one**,
            // which is exactly the transition the barrier exists to notice.
            if let Some(upvalue) = self.heap.upvalue_mut(id) {
                upvalue.closed = value;
            }
            self.heap.wrote(id, value);
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
    /// only live references are the ones below.
    ///
    /// Collect the young generation.
    ///
    /// **The handles go into a buffer the VM keeps.** This runs whenever the
    /// nursery fills, which is often, and an allocation per collection is the
    /// sort of cost that eats what a nursery is supposed to save.
    fn collect_young(&mut self) {
        let roots = self.roots();
        let mut buffer = ::core::mem::take(&mut self.root_handles);
        buffer.clear();
        buffer.extend(roots.iter().filter_map(|value| value.as_object()));
        self.heap.collect_minor(&buffer);
        buffer.clear();
        self.root_handles = buffer;
    }

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
        // **The running fiber, and through it the chain of its callers.**
        // While a fiber runs, its stack and frames live on the VM rather than
        // in the object, so the object itself is referenced from nowhere else
        // -- the root fiber in particular, which no program ever names.
        // Freeing it made `Fiber.yield` unable to find who to go back to, and
        // reported as "Not a fiber" five tests later. `Object::trace` follows
        // `caller` from here.
        if let Some(id) = self.current_fiber {
            roots.push(Value::object(id));
        }
        // The core classes are reachable from nothing else once a program has
        // stopped mentioning them by name, and freeing `Num` mid-program would
        // be spectacular.
        for class in [
            self.num_class,
            self.bool_class,
            self.null_class,
            self.string_class,
            self.list_class,
            self.map_class,
            self.range_class,
            self.class_class,
            self.object_class,
            self.fn_class,
            self.map_entry_class,
            self.fiber_class,
            self.random_class,
            self.sequence_class,
            self.map_sequence_class,
            self.where_sequence_class,
            self.take_sequence_class,
            self.skip_sequence_class,
            self.map_key_sequence_class,
            self.map_value_sequence_class,
            self.class_attributes_class,
            self.string_byte_sequence_class,
            self.string_code_point_sequence_class,
        ] {
            roots.push(Value::object(class));
        }
        roots
    }

    /// A value as text, dispatching `toString` so a class's own conversion is
    /// used when it has one.
    pub fn stringify(&mut self, value: Value) -> Result<String, RuntimeError> {
        let text = self.invoke(value, "toString")?;
        // **A `toString` that returns something other than a string is not an
        // error.** Wren prints a placeholder, because failing here would mean
        // a debugging `System.print` could itself raise -- exactly when the
        // program is already misbehaving.
        if !self.is_string(text) {
            return Ok("[invalid toString]".to_string());
        }
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
        if let Some(fiber) = self.heap.fiber_mut(id) {
            fiber.stack = stack;
            fiber.frames = frames;
        }
    }

    /// Take a fiber's stack and frames as the VM's own, and run it.
    fn resume(&mut self, target: ObjectId, value: Value) -> Result<(), RuntimeError> {
        let (mut stack, mut frames, entry, done) = match self.heap.fiber_mut(target) {
            Some(fiber) => (
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
            let target = self.call_target(entry)?;
            frames.push(Frame {
                closure: entry,
                ip: 0,
                base: 0,
                chunk: target.chunk,
                module: target.module,
                field_offset: target.field_offset,
            });
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
            if let Some(fiber) = self.heap.fiber_mut(switch.target) {
                fiber.caller = from;
                fiber.catching = switch.catching;
            }
        }
        if switch.finishing {
            if let Some(from) = from {
                if let Some(fiber) = self.heap.fiber_mut(from) {
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
            let fiber = self.heap.fiber(id)?;
            if fiber.catching {
                return fiber.caller;
            }
            current = fiber.caller;
        }
        None
    }

    /// Call a method with arguments on a value, from Rust.
    ///
    /// This is what lets the `Sequence` methods be written in Rust and still
    /// work on any type that answers the iteration protocol -- including a
    /// class defined in Wren, whose `iterate` is a closure this has to call.
    pub fn invoke_with(
        &mut self,
        receiver: Value,
        signature: &str,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let Some(symbol) = self.method_names.find(signature) else {
            return Err(RuntimeError::new(format!(
                "{} does not implement '{signature}'.",
                self.class_of(receiver)
                    .map(|class| self.class_name(class))
                    .unwrap_or_else(|| "null".to_string())
            )));
        };
        let Some(class) = self.class_of(receiver) else {
            return Err(RuntimeError::new("Receiver has no class."));
        };
        let Some(method) = self.find_method(class, symbol) else {
            return Err(RuntimeError::new(format!(
                "{} does not implement '{signature}'.",
                self.class_name(class)
            )));
        };

        let at = self.stack.len();
        self.stack.push(receiver);
        for argument in args {
            self.stack.push(*argument);
        }
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
    pub fn call_function(
        &mut self,
        function: Value,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let Some(closure) = function.as_object() else {
            return Err(RuntimeError::new("Argument must be a function."));
        };
        if self.heap.closure(closure).is_none() {
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

    /// Run a closure that was loaded rather than compiled.
    ///
    /// The entry point for a part with no compiler: everything `interpret`
    /// does after the parse, and nothing it does before.
    pub fn run_closure(&mut self, closure: ObjectId) -> Result<(), RuntimeError> {
        if self.current_fiber.is_none() {
            let root = self
                .heap
                .allocate(Object::Fiber(Box::new(ObjFiber::new(closure))));
            self.current_fiber = Some(root);
            self.root_fiber = Some(root);
        }

        let base = self.stack.len();
        self.stack.push(Value::NULL);
        let depth = self.frames.len();
        let chunk = self.push_frame(closure, base)?;
        self.run_frames(chunk, depth)?;
        Ok(())
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
            owner_class: None,
            module,
        })));
        let closure = self.heap.allocate(Object::Closure(ObjClosure {
            function,
            upvalues: Vec::new(),
        }));

        // The module runs in a root fiber, so that `Fiber.yield` at the top
        // level has something to complain about and `Fiber.current` has an
        // answer.
        if self.current_fiber.is_none() {
            let root = self
                .heap
                .allocate(Object::Fiber(Box::new(ObjFiber::new(closure))));
            self.current_fiber = Some(root);
            self.root_fiber = Some(root);
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
            // **The offset, not the line.** `line_at` is a lookup into a table
            // as long as the code, and the line is wanted only when something
            // fails -- which is never, in the overwhelming majority of
            // instructions. Keeping the offset costs a register; looking the
            // line up cost a bounds check and a load on every instruction
            // executed.
            let at = ip;
            let byte = chunk.code[at];
            ip += 1;

            let Some(op) = Op::from_byte(byte) else {
                return Err(RuntimeError {
                    message: format!("bad opcode {byte}"),
                    line: chunk.line_at(at),
                });
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
                        .allocate(Object::Closure(ObjClosure { function, upvalues }));
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
                    // **The value is on top, the receiver below it.** The
                    // compiler pushes `this` first and then evaluates the
                    // right-hand side, so popping the receiver first took the
                    // value and stored the instance into itself. Assignment is
                    // an expression, so the value is what stays.
                    let value = self.stack.pop().unwrap_or(Value::NULL);
                    let receiver = self.stack.pop().unwrap_or(Value::NULL);
                    self.set_field(receiver, base, index, value)?;
                    self.stack.push(value);
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
                            line: chunk.line_at(at),
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
                                    error.line = chunk.line_at(at);
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
                            if self.halting {
                                self.halting = false;
                                return Ok(Value::NULL);
                            }
                            if let Some(switch) = self.pending_switch.take() {
                                let failing = switch.as_error.then_some(switch.value);
                                self.perform_switch(switch, ip)?;
                                if let Some(value) = failing {
                                    // The target fails the moment it resumes,
                                    // which is what makes `transferError`
                                    // different from `transfer`.
                                    let message = self.to_string(value);
                                    let error = RuntimeError {
                                        message,
                                        line: chunk.line_at(at),
                                    };
                                    match self.deliver_error(error, 0)? {
                                        Some((next_chunk, next_ip, next_base)) => {
                                            chunk = next_chunk;
                                            ip = next_ip;
                                            base = next_base;
                                            module = self.current_module();
                                            continue;
                                        }
                                        None => unreachable!("deliver_error returns or switches"),
                                    }
                                }
                                let frame = self.frames.last().expect("a frame to resume");
                                ip = frame.ip;
                                base = frame.base;
                                chunk = frame.chunk.clone();
                                module = frame.module;
                                continue;
                            }
                            self.stack.push(value);
                        }
                        Method::Closure(closure) => {
                            // **One walk to the closure, not three.** Arity,
                            // code and module all come from the same `ObjFn`,
                            // and asking for them separately was the bulk of
                            // what a call cost.
                            let target = self.call_target(closure)?;
                            if target.arity != arity {
                                return Err(RuntimeError {
                                    message: format!(
                                        "Function expects {} argument(s) but got {arity}.",
                                        target.arity
                                    ),
                                    line: chunk.line_at(at),
                                });
                            }
                            if self.frames.len() >= MAX_FRAMES {
                                return Err(RuntimeError {
                                    message: "Stack overflow.".into(),
                                    line: chunk.line_at(at),
                                });
                            }
                            if let Some(frame) = self.frames.last_mut() {
                                frame.ip = ip;
                            }
                            self.frames.push(Frame {
                                closure,
                                ip: 0,
                                base: receiver_at,
                                chunk: target.chunk.clone(),
                                module: target.module,
                                field_offset: target.field_offset,
                            });
                            chunk = target.chunk;
                            ip = 0;
                            base = receiver_at;
                            module = target.module;
                        }
                    }
                }
                Op::LoadStaticField => {
                    let index = chunk.code[ip] as usize;
                    ip += 1;
                    let value = self.static_field(index);
                    self.stack.push(value);
                }
                Op::StoreStaticField => {
                    let index = chunk.code[ip] as usize;
                    ip += 1;
                    let value = *self.stack.last().unwrap();
                    self.set_static_field(index, value)?;
                }
                Op::SetAttributes => {
                    let attributes = self.stack.pop().unwrap_or(Value::NULL);
                    let class = self.stack.pop().unwrap_or(Value::NULL);
                    if let Some(id) = class.as_object() {
                        if let Some(class) = self.heap.class_mut(id) {
                            class.attributes = attributes;
                        }
                        self.heap.wrote(id, attributes);
                    }
                }
                Op::ImportModule => {
                    let index = chunk.read_short(ip) as usize;
                    ip += 2;
                    let name = self.to_string(chunk.constants[index]);
                    // Resolved against the module doing the importing.
                    let name = resolve_module(&self.modules[module].name, &name);

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
                            // **Assigned, not `let`.** A fresh binding here
                            // would shadow the loop's `base` for this arm only,
                            // so the module body would run against the caller's
                            // base and its return would truncate the stack past
                            // the caller's own slots. The symptom was a `for`
                            // loop after any import reading its hidden
                            // iterator local as null.
                            base = self.stack.len();
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
                    // The same resolution, or the `for` clause would look the
                    // module up under the name as written rather than the one
                    // it was loaded under.
                    let module_name = resolve_module(&self.modules[module].name, &module_name);
                    let variable = self.to_string(chunk.constants[variable_name]);

                    let Some(from) = self.module_index.get(&module_name).copied() else {
                        return Err(RuntimeError {
                            message: format!("Could not load module '{module_name}'."),
                            line: chunk.line_at(at),
                        });
                    };
                    let Some(value) = self.modules[from].get(&variable) else {
                        return Err(RuntimeError {
                            message: format!(
                                "Could not find a variable named '{variable}' in module '{module_name}'."
                            ),
                            line: chunk.line_at(at),
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
                        // **A fiber is finished when it has no frames left**,
                        // not when this particular `run_frames` returns. A
                        // primitive that re-enters the interpreter -- `Fn.call`,
                        // or any Sequence method taking a block -- returns
                        // through here too, and marking the fiber done then
                        // ended the fiber that merely *contained* the call.
                        // The symptom was a second `fiber.call()` reporting the
                        // fiber already finished after it had only yielded.
                        let finished = self.frames.is_empty();
                        let caller = self.current_fiber.and_then(|id| match self.heap.fiber(id) {
                            Some(fiber) => fiber.caller,
                            _ => None,
                        });
                        if let (Some(caller), true) = (caller, finished) {
                            self.perform_switch(
                                Switch {
                                    target: caller,
                                    value: result,
                                    set_caller: false,
                                    catching: false,
                                    finishing: true,
                                    as_error: false,
                                },
                                ip,
                            )?;
                            let frame = self.frames.last().expect("a frame to resume");
                            ip = frame.ip;
                            base = frame.base;
                            chunk = frame.chunk.clone();
                            module = frame.module;
                            continue;
                        }
                        if finished {
                            if let Some(id) = self.current_fiber {
                                if let Some(fiber) = self.heap.fiber_mut(id) {
                                    fiber.done = true;
                                }
                            }
                        }
                        return Ok(result);
                    }

                    self.stack.push(result);
                    // **The return path.** This ran twice per call before --
                    // closure to function to chunk, and again for the module --
                    // and is now two field reads and a refcount bump.
                    let frame = self.frames.last().expect("a frame to return to");
                    ip = frame.ip;
                    base = frame.base;
                    chunk = frame.chunk.clone();
                    module = frame.module;
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
            //
            // **The nursery is asked first.** A minor collection costs the
            // young generation; a major costs the live set. Checking the major
            // threshold first meant it always won -- the heap passes 1.5x its
            // live size long before a thousand objects accumulate -- and the
            // nursery never collected at all.
            if NURSERY && self.heap.young() >= NURSERY_OBJECTS {
                // **The cheap half of collection**, at the same safe point and
                // for the same reason. A minor collection costs the young
                // generation and the remembered set rather than the live set,
                // so it is worth doing often -- and 84% of what it looks at is
                // already dead.
                if let Some(frame) = self.frames.last_mut() {
                    frame.ip = ip;
                }
                self.collect_young();
            } else if self.heap.should_collect() {
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

        // **Every fiber between the one that failed and the one entered with
        // `try` is aborted too, carrying the same error.** A fiber that was
        // only passed through is as dead as the one that raised: it can never
        // be resumed, because the frame it was waiting on is gone. Marking
        // only the innermost left the intermediates reporting `null` from
        // `fiber.error` while also claiming not to be done.
        let mut current = self.current_fiber;
        while let Some(id) = current {
            let Some(fiber) = self.heap.fiber_mut(id) else {
                break;
            };
            fiber.error = message;
            fiber.done = true;
            let caller = fiber.caller;
            if fiber.catching {
                // This one's caller is where control is going; it keeps its
                // link so the hand-back lands there.
                break;
            }
            // Never resumed, so unhook it.
            fiber.caller = None;
            current = caller;
        }

        self.perform_switch(
            Switch {
                target: catcher,
                value: message,
                set_caller: false,
                catching: false,
                finishing: false,
                as_error: false,
            },
            ip,
        )?;
        let frame = self.frames.last().expect("a frame to resume");
        Ok(Some((frame.chunk.clone(), frame.ip, frame.base)))
    }

    fn read_upvalue(&self, base: usize, slot: usize) -> Result<Value, RuntimeError> {
        let _ = base;
        let Some(frame) = self.frames.last() else {
            return Err(RuntimeError::new("No frame."));
        };
        let Some(closure) = self.heap.closure(frame.closure) else {
            return Err(RuntimeError::new("No closure."));
        };
        let Some(id) = closure.upvalues.get(slot).copied() else {
            return Err(RuntimeError::new("No such upvalue."));
        };
        match self.heap.upvalue(id) {
            Some(upvalue) => {
                if upvalue.is_open() {
                    Ok(self
                        .stack
                        .get(upvalue.slot())
                        .copied()
                        .unwrap_or(Value::NULL))
                } else {
                    Ok(upvalue.closed)
                }
            }
            _ => Err(RuntimeError::new("Not an upvalue.")),
        }
    }

    fn write_upvalue(&mut self, slot: usize, value: Value) -> Result<(), RuntimeError> {
        let Some(frame) = self.frames.last() else {
            return Err(RuntimeError::new("No frame."));
        };
        let Some(closure) = self.heap.closure(frame.closure) else {
            return Err(RuntimeError::new("No closure."));
        };
        let Some(id) = closure.upvalues.get(slot).copied() else {
            return Err(RuntimeError::new("No such upvalue."));
        };
        let target = match self.heap.upvalue(id) {
            Some(upvalue) => upvalue.is_open().then_some(upvalue.slot()),
            _ => return Err(RuntimeError::new("Not an upvalue.")),
        };
        match target {
            Some(stack_slot) => {
                if stack_slot < self.stack.len() {
                    self.stack[stack_slot] = value;
                }
            }
            None => {
                if let Some(upvalue) = self.heap.upvalue_mut(id) {
                    upvalue.closed = value;
                }
                self.heap.wrote(id, value);
            }
        }
        Ok(())
    }

    fn upvalue_of_current(&self, index: usize) -> Result<ObjectId, RuntimeError> {
        let Some(frame) = self.frames.last() else {
            return Err(RuntimeError::new("No frame."));
        };
        let Some(closure) = self.heap.closure(frame.closure) else {
            return Err(RuntimeError::new("No closure."));
        };
        closure
            .upvalues
            .get(index)
            .copied()
            .ok_or_else(|| RuntimeError::new("No such upvalue."))
    }

    fn class_name(&self, class: ObjectId) -> String {
        match self.heap.class(class) {
            Some(class) => match self.heap.string(class.name) {
                Some(name) => name.as_str().unwrap_or("?").to_string(),
                _ => "?".to_string(),
            },
            _ => "?".to_string(),
        }
    }

    /// The field offset the currently running method was bound with.
    fn field_of(&self, receiver: Value, _base: usize, index: usize) -> Result<Value, RuntimeError> {
        let offset = self.current_field_offset();
        let Some(id) = receiver.as_object() else {
            return Err(RuntimeError::new(
                "Cannot access a field outside of a class.",
            ));
        };
        match self.heap.instance(id) {
            Some(_) => Ok(self
                .heap
                .instance_field(id, offset + index)
                .unwrap_or(Value::NULL)),
            None => Err(RuntimeError::new(
                "Cannot access a field outside of a class.",
            )),
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
            return Err(RuntimeError::new(
                "Cannot access a field outside of a class.",
            ));
        };
        match self.heap.instance(id) {
            Some(_) => {
                // The store and the barrier both live in the heap now, because
                // the fields do.
                self.heap.set_instance_field(id, offset + index, value);
                Ok(())
            }
            None => Err(RuntimeError::new(
                "Cannot access a field outside of a class.",
            )),
        }
    }

    /// The class whose static fields the running method should see.
    fn owner_class(&self) -> Option<ObjectId> {
        self.frames
            .last()
            .and_then(|frame| self.function_of(frame.closure))
            .and_then(|function| function.owner_class)
    }

    fn static_field(&self, index: usize) -> Value {
        // Unset reads as null rather than as an error, which is what
        // `use_before_set` expects: a static field springs into existence the
        // first time it is mentioned.
        let Some(owner) = self.owner_class() else {
            return Value::NULL;
        };
        match self.heap.class(owner) {
            Some(class) => class
                .static_fields
                .get(index)
                .copied()
                .unwrap_or(Value::NULL),
            _ => Value::NULL,
        }
    }

    fn set_static_field(&mut self, index: usize, value: Value) -> Result<(), RuntimeError> {
        let Some(owner) = self.owner_class() else {
            return Err(RuntimeError::new(
                "Cannot use a static field outside of a class definition.",
            ));
        };
        if let Some(class) = self.heap.class_mut(owner) {
            if class.static_fields.len() <= index {
                class.static_fields.resize(index + 1, Value::NULL);
            }
            class.static_fields[index] = value;
        }
        self.heap.wrote(owner, value);
        Ok(())
    }

    fn current_field_offset(&self) -> usize {
        self.frames
            .last()
            .map_or(0, |frame| frame.field_offset as usize)
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
        let inherited = match self.heap.class(superclass_id) {
            Some(class) => class.num_fields.max(0) as usize,
            _ => return Err(RuntimeError::new("Class must inherit from a class.")),
        };

        // **The built-ins cannot be subclassed.** Their instances have a
        // representation of their own -- a `Num` is a double in the value
        // itself, a `List` is a vector -- and an instance of a subclass would
        // have to be both that and an object with fields. Upstream refuses for
        // the same reason and with this wording.
        let builtin = [
            self.bool_class,
            self.class_class,
            self.fiber_class,
            self.fn_class,
            self.list_class,
            self.map_class,
            self.null_class,
            self.num_class,
            self.range_class,
            self.string_class,
        ];
        if builtin.contains(&superclass_id) {
            let child = self.to_string(name);
            let parent = self.class_name(superclass_id);
            return Err(RuntimeError::new(format!(
                "Class '{child}' cannot inherit from built-in class '{parent}'."
            )));
        }

        let Some(name_id) = name.as_object() else {
            return Err(RuntimeError::new("Class name must be a string."));
        };

        // A metaclass, so the class can carry static methods and a constructor.
        let class_name = match self.heap.string(name_id) {
            Some(text) => text.as_str().unwrap_or("?").to_string(),
            _ => "?".to_string(),
        };
        let metaclass_name = self
            .heap
            .allocate(Object::String(ObjString::from_text(&format!(
                "{class_name} metaclass"
            ))));
        let metaclass = self.heap.allocate(Object::Class(Box::new(ObjClass::new(
            metaclass_name,
            Some(self.class_class),
        ))));

        // 255 fields, inherited ones included: a field index is a byte in the
        // bytecode, so this is the representation's limit rather than a policy.
        if inherited + declared > 255 {
            let child = self.to_string(name);
            return Err(RuntimeError::new(format!(
                "Class '{child}' may not have more than 255 fields, including inherited ones."
            )));
        }

        let mut class = ObjClass::new(name_id, Some(superclass_id));
        class.num_fields = (inherited + declared) as i32;
        class.metaclass = Some(metaclass);
        let class_id = self.heap.allocate(Object::Class(Box::new(class)));

        // **Inherit before the body binds anything.** The class body's methods
        // are bound after this returns, so they land on top of the copied ones
        // and an override wins by being written second.
        self.inherit_methods(class_id, superclass_id);
        // A metaclass inherits from `Class` the same way, which is what makes
        // `SomeClass.name` and `SomeClass.toString` work without a chain walk.
        let class_class = self.class_class;
        self.inherit_methods(metaclass, class_class);

        Ok(Value::object(class_id))
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

        // **Which class the method lands on is decided first**, because
        // everything else follows from it. A static method is installed on the
        // metaclass, so its `super` must start from the *metaclass's*
        // superclass -- `Class` -- and not from the class's. Deriving `super`
        // from `class_id` sent `super.name` in a static method looking through
        // `Object`, where `name` is not, rather than through `Class`, where it
        // is.
        let target = if is_static {
            match self.heap.class(class_id) {
                Some(class) => class.metaclass.unwrap_or(class_id),
                _ => class_id,
            }
        } else {
            class_id
        };

        let superclass = match self.heap.class(target) {
            Some(class) => class.superclass,
            _ => None,
        };

        // **Where the field offset is filled in.** The compiler numbered this
        // method's fields from zero; now that the class is known, so is how
        // many fields the superclass already occupies.
        let inherited = match superclass {
            Some(superclass) => match self.heap.class(superclass) {
                Some(superclass) => superclass.num_fields.max(0) as usize,
                _ => 0,
            },
            None => 0,
        };
        self.set_field_offset(closure, inherited, superclass, class_id);

        if let Some(class) = self.heap.class_mut(target) {
            class.define(symbol, crate::object::closure_entry(closure));
        }
        // A method table entry is a reference like any other: a class that has
        // already been collected once now points at a freshly compiled body.
        self.heap.wrote(target, Value::object(closure));
        Ok(())
    }

    /// Set a closure's field offset, and every function nested inside it.
    ///
    /// A closure written inside a method still refers to the same fields, so
    /// the offset has to reach it too — upstream walks nested functions at bind
    /// time for the same reason.
    fn set_field_offset(
        &mut self,
        closure: ObjectId,
        offset: usize,
        superclass: Option<ObjectId>,
        owner: ObjectId,
    ) {
        let Some(closure) = self.heap.closure(closure) else {
            return;
        };
        let function = closure.function;
        let nested: Vec<ObjectId> = match self.heap.function(function) {
            Some(function) => function
                .chunk
                .constants
                .iter()
                .filter_map(|constant| constant.as_object())
                .filter(|id| self.heap.function(*id).is_some())
                .collect(),
            _ => Vec::new(),
        };
        if let Some(function) = self.heap.function_mut(function) {
            function.field_offset = offset;
            function.super_class = superclass;
            function.owner_class = Some(owner);
        }
        for id in nested {
            self.set_field_offset_of_fn(id, offset, superclass, owner);
        }
    }

    fn set_field_offset_of_fn(
        &mut self,
        function: ObjectId,
        offset: usize,
        superclass: Option<ObjectId>,
        owner: ObjectId,
    ) {
        let nested: Vec<ObjectId> = match self.heap.function(function) {
            Some(function) => function
                .chunk
                .constants
                .iter()
                .filter_map(|constant| constant.as_object())
                .filter(|id| self.heap.function(*id).is_some())
                .collect(),
            _ => return,
        };
        if let Some(function) = self.heap.function_mut(function) {
            function.field_offset = offset;
            function.super_class = superclass;
            function.owner_class = Some(owner);
        }
        for id in nested {
            self.set_field_offset_of_fn(id, offset, superclass, owner);
        }
    }

    fn instantiate(&mut self, class: Value) -> Result<Value, RuntimeError> {
        let Some(class_id) = class.as_object() else {
            return Err(RuntimeError::new("Not a class."));
        };
        let fields = match self.heap.class(class_id) {
            Some(class) => class.num_fields.max(0) as usize,
            _ => return Err(RuntimeError::new("Not a class.")),
        };
        let id = self
            .heap
            .new_instance(class_id, &alloc::vec![Value::NULL; fields]);
        Ok(Value::object(id))
    }
}

impl Default for Vm {
    fn default() -> Vm {
        Vm::new()
    }
}

/// Resolve an import against the module doing the importing.
///
/// A name starting with `./` or `../` is relative; anything else -- `random`,
/// `meta` -- is a logical name and passes through untouched. **This cannot be
/// left to the host loader**, because the loader is handed one name and never
/// learns which module asked for it.
///
/// A leading `..` that cannot be resolved is kept rather than discarded, so an
/// import reaching above the root fails to load with the name it asked for
/// instead of quietly becoming something else.
pub fn resolve_module(importer: &str, name: &str) -> String {
    if !name.starts_with("./") && !name.starts_with("../") {
        return name.to_string();
    }

    let directory = match importer.rfind('/') {
        Some(at) => &importer[..at],
        None => "",
    };
    let joined = if directory.is_empty() {
        name.to_string()
    } else {
        format!("{directory}/{name}")
    };

    let mut parts: Vec<&str> = Vec::new();
    let mut above = 0usize;
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    above += 1;
                }
            }
            other => parts.push(other),
        }
    }

    let mut resolved = String::new();
    for _ in 0..above {
        if !resolved.is_empty() {
            resolved.push('/');
        }
        resolved.push_str("..");
    }
    // A path that stayed inside its own directory keeps the leading `./` it
    // was written with, which is what the host loader expects to strip.
    if above == 0 && joined.starts_with("./") {
        resolved.push('.');
    }
    for part in parts {
        if !resolved.is_empty() {
            resolved.push('/');
        }
        resolved.push_str(part);
    }
    if resolved.is_empty() {
        resolved.push('.');
    }
    resolved
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
fn format_number(value: crate::value::Num) -> String {
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
    let exponential = format!("{:.*e}", SIGNIFICANT - 1, value);
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
    if exponent < -4 || exponent >= SIGNIFICANT as i32 {
        let mantissa = trim_trailing_zeros(mantissa);
        let sign = if exponent < 0 { '-' } else { '+' };
        // C pads the exponent to at least two digits: `1e+05`, not `1e+5`.
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }

    // Decimal: N significant digits means N - 1 - exponent after the point.
    let decimals = (SIGNIFICANT as i32 - 1 - exponent).max(0) as usize;
    trim_trailing_zeros(&format!("{value:.decimals$}"))
}

/// How many significant digits a number prints to.
///
/// **Fourteen is upstream's `%.14g`**, and the reason is readability rather
/// than round-tripping: `0.1 + 0.2` is `0.30000000000000004` at full precision
/// and `0.3` at fourteen digits, and the second is what a person writing a
/// script means.
///
/// A 32-bit build uses eight, which is the same argument at the narrower
/// width. Seven would send any integer above 9,999,999 into exponential form
/// while an `f32` still holds integers exactly to 16,777,216; nine would stop
/// `0.1` printing as `0.1`. Eight is the value that keeps both.
#[cfg(not(feature = "f32"))]
const SIGNIFICANT: usize = 14;
#[cfg(feature = "f32")]
const SIGNIFICANT: usize = 8;

/// Strip the trailing zeros `%g` removes, and the point if nothing follows it.
fn trim_trailing_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}
