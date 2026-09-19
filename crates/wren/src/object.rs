//! Heap objects, and the handles that refer to them.
//!
//! Everything that is not a number, a boolean or null lives here. Upstream
//! calls these `Obj` and reaches them through `Obj*`; this reaches them through
//! an [`ObjectId`] indexing a table the [`Heap`](crate::heap::Heap) owns. See
//! `doc/wren-rs/design.md` for why, and what it costs.

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::bytecode::Chunk;

use crate::value::Value;

pub use crate::handle::ObjectId;

/// Which kind of object a handle refers to.
///
/// The names and the order are upstream's `ObjType`, including the members
/// this implementation has not built yet, so that the two enumerations can be
/// compared without a mapping table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectType {
    Class,
    Closure,
    Fn,
    Instance,
    List,
    Map,
    Range,
    String,
    Upvalue,
    Fiber,
}

/// A heap object.
///
/// **Only the six types the collector can meaningfully trace today are here.**
/// Upstream also has `Closure`, `Fiber`, `Fn`, `Foreign`, `Module` and
/// `Upvalue`; those arrive with the compiler that gives them something to hold,
/// rather than as empty structs now. A type that exists but is never populated
/// reads as finished work and is not.
///
/// # A note on size
///
/// Every slot in the heap's table is one `Object`, so **every object costs as
/// much as the largest variant** — 24 bytes on a 32-bit part, set by `Range`'s
/// two `f64`s, against 12 for a `List`. Upstream allocates each type at its own
/// size and does not pay this.
///
/// It is left this way on purpose for now. The alternative — one table per
/// type, with the type encoded in the handle's high bits — removes the waste
/// *and* makes type checks free, but it is six free lists instead of one and a
/// good deal more code. The benchmark set exists to decide that with a
/// measurement rather than an intuition; see `doc/wren-rs/design.md`.
#[derive(Debug)]
pub enum Object {
    /// **Boxed, unlike the others.** A class carries a method table, which
    /// makes `ObjClass` far larger than any other payload -- large enough to
    /// set the size of every slot in the heap, since a slot is one `Object`.
    /// Boxing it puts a pointer in the slot instead and leaves the other
    /// objects at 24 bytes.
    ///
    /// The trade is one more indirection to reach a class. Classes are few and
    /// long-lived where ranges and strings are many and short-lived, so this is
    /// the right way round -- but it is a real cost on the dispatch path, which
    /// reaches a class on every single method call.
    Class(Box<ObjClass>),
    /// A compiled function body. Boxed for the same reason a class is.
    Fn(Box<ObjFn>),
    /// A function plus the variables it captured.
    Closure(Box<ObjClosure>),
    /// One captured variable. See [`ObjUpvalue`].
    Upvalue(ObjUpvalue),
    /// A coroutine with its own stack and call frames. See [`ObjFiber`].
    Fiber(Box<ObjFiber>),
    Instance(ObjInstance),
    List(ObjList),
    Map(ObjMap),
    Range(ObjRange),
    String(ObjString),
}

impl Object {
    pub fn object_type(&self) -> ObjectType {
        match self {
            Object::Class(_) => ObjectType::Class,
            Object::Closure(_) => ObjectType::Closure,
            Object::Fn(_) => ObjectType::Fn,
            Object::Upvalue(_) => ObjectType::Upvalue,
            Object::Fiber(_) => ObjectType::Fiber,
            Object::Instance(_) => ObjectType::Instance,
            Object::List(_) => ObjectType::List,
            Object::Map(_) => ObjectType::Map,
            Object::Range(_) => ObjectType::Range,
            Object::String(_) => ObjectType::String,
        }
    }

    /// Push every object this one refers to onto the collector's work list.
    ///
    /// This is the whole of the mark phase's knowledge of the object graph.
    /// **A type that forgets to report a reference here is a use-after-free in
    /// any other language and a silently missing object in this one** — the
    /// handle survives, the lookup fails. Adding a field that holds a `Value`
    /// or an `ObjectId` means adding it here.
    pub fn trace(&self, gray: &mut Vec<ObjectId>) {
        match self {
            Object::Class(class) => {
                gray.push(class.name);
                if let Some(superclass) = class.superclass {
                    gray.push(superclass);
                }
                // A class's metaclass holds its static methods, and nothing
                // else refers to it. Forgetting this frees the metaclass out
                // from under a class that is still in use.
                if let Some(metaclass) = class.metaclass {
                    gray.push(metaclass);
                }
                // **And the methods themselves.** A method written in Wren is
                // a closure the method table is the only reference to -- once
                // the class definition has finished executing, the closure is
                // gone from the stack. Omitting this compiled and ran and
                // passed every small test, because nothing collected before
                // the program was over; it appeared the moment a second class
                // pushed the heap past its first collection, and presented as
                // the *first* class's constructor silently doing nothing.
                //
                // This is the failure mode the note on `trace` describes, and
                // it is worth having actually happened: the omission is
                // invisible until a collection runs at exactly the wrong
                // moment, which is the hardest kind of bug to go looking for.
                for method in class.methods.iter().flatten() {
                    match method {
                        Method::Closure(closure) => gray.push(*closure),
                        // A primitive is a Rust function pointer with no heap
                        // object behind it, so there is nothing to keep alive.
                        Method::Primitive(_) => {}
                    }
                }
            }
            Object::Fn(function) => {
                // **A function's constants are references like any other.** A
                // string literal in a function body is a heap object reachable
                // only from here; missing it frees the literal out from under
                // code that is about to load it.
                for constant in &function.chunk.constants {
                    if let Some(id) = constant.as_object() {
                        gray.push(id);
                    }
                }
            }
            Object::Closure(closure) => {
                gray.push(closure.function);
                for upvalue in &closure.upvalues {
                    gray.push(*upvalue);
                }
            }
            Object::Upvalue(upvalue) => {
                if let Some(value) = upvalue.closed {
                    if let Some(id) = value.as_object() {
                        gray.push(id);
                    }
                }
            }
            Object::Fiber(fiber) => {
                // A suspended fiber's stack is live even though nothing is
                // running on it; its frames hold the only reference to the
                // closures half way through executing.
                for value in &fiber.stack {
                    if let Some(id) = value.as_object() {
                        gray.push(id);
                    }
                }
                for frame in &fiber.frames {
                    gray.push(frame.closure);
                }
                if let Some(caller) = fiber.caller {
                    gray.push(caller);
                }
                if let Some(id) = fiber.error.as_object() {
                    gray.push(id);
                }
                if let Some(entry) = fiber.entry {
                    gray.push(entry);
                }
            }
            Object::Instance(instance) => {
                gray.push(instance.class);
                for field in &instance.fields {
                    if let Some(id) = field.as_object() {
                        gray.push(id);
                    }
                }
            }
            Object::List(list) => {
                for element in &list.elements {
                    if let Some(id) = element.as_object() {
                        gray.push(id);
                    }
                }
            }
            Object::Map(map) => {
                for entry in &map.entries {
                    if let Some(id) = entry.key.as_object() {
                        gray.push(id);
                    }
                    if let Some(id) = entry.value.as_object() {
                        gray.push(id);
                    }
                }
            }
            // Neither holds a reference: a range is two numbers, and a string
            // owns its bytes outright.
            Object::Range(_) => {}
            Object::String(_) => {}
        }
    }

    /// Roughly what this object costs, for the collector's growth heuristic.
    ///
    /// Upstream tracks bytes allocated exactly, because it does every
    /// allocation itself. Here the `Vec`s inside an object allocate on their
    /// own, so this is the slot plus whatever those have reserved — close
    /// enough to decide when to collect, and not claimed to be more.
    pub fn size_estimate(&self) -> usize {
        let slot = core::mem::size_of::<Object>();
        let inner = match self {
            Object::Class(class) => {
                core::mem::size_of::<ObjClass>()
                    + class.methods.capacity() * core::mem::size_of::<Option<Method>>()
            }
            Object::Instance(instance) => {
                instance.fields.capacity() * core::mem::size_of::<Value>()
            }
            Object::List(list) => list.elements.capacity() * core::mem::size_of::<Value>(),
            Object::Map(map) => map.entries.capacity() * core::mem::size_of::<MapEntry>(),
            Object::Range(_) => 0,
            Object::String(string) => string.bytes.capacity(),
            // The chunk is shared through an `Rc`, so charging its full size
            // to every closure over it would count the same bytes many times.
            Object::Fn(_) => core::mem::size_of::<ObjFn>(),
            Object::Closure(closure) => {
                core::mem::size_of::<ObjClosure>()
                    + closure.upvalues.capacity() * core::mem::size_of::<ObjectId>()
            }
            Object::Upvalue(_) => 0,
            Object::Fiber(fiber) => {
                core::mem::size_of::<ObjFiber>()
                    + fiber.stack.capacity() * core::mem::size_of::<Value>()
            }
        };
        slot + inner
    }
}

/// A string.
///
/// Wren strings are **byte strings that are usually UTF-8**, not Rust `String`s.
/// A Wren program can build one byte by byte with `String.fromByte`, and
/// upstream neither validates nor rejects the result. Storing `Vec<u8>` rather
/// than `String` is what keeps that behaviour reachable; the places that need
/// characters rather than bytes decode on the way past.
#[derive(Debug)]
pub struct ObjString {
    pub bytes: Vec<u8>,
    /// Cached because maps hash their keys on every lookup and rehashing a
    /// long string each time is what makes a naive implementation slow.
    /// Upstream caches it in the same place for the same reason.
    hash: u32,
}

impl ObjString {
    pub fn new(bytes: Vec<u8>) -> ObjString {
        let hash = hash_bytes(&bytes);
        ObjString { bytes, hash }
    }

    pub fn from_text(text: &str) -> ObjString {
        ObjString::new(text.as_bytes().to_vec())
    }

    pub fn hash(&self) -> u32 {
        self.hash
    }

    /// The bytes as `&str`, when they happen to be valid UTF-8.
    ///
    /// Returns `None` rather than replacing anything, because a caller that
    /// wants lossy behaviour should ask for it explicitly.
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.bytes).ok()
    }
}

/// FNV-1a, which is what upstream uses.
///
/// Kept identical so that anything depending on iteration order — map output
/// in a test, say — behaves the same in both, and a divergence is a real
/// difference rather than a different hash function.
fn hash_bytes(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 2166136261;
    for byte in bytes {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

/// A list.
#[derive(Debug)]
pub struct ObjList {
    pub elements: Vec<Value>,
}

impl ObjList {
    pub fn new() -> ObjList {
        ObjList { elements: Vec::new() }
    }
}

impl Default for ObjList {
    fn default() -> ObjList {
        ObjList::new()
    }
}

/// One key/value pair in a map.
#[derive(Clone, Copy, Debug)]
pub struct MapEntry {
    pub key: Value,
    pub value: Value,
}

/// A map.
///
/// **The lookup here is a linear scan, and that is temporary.** Upstream uses
/// open addressing with a power-of-two capacity, which needs a key's hash — and
/// hashing a string key means reaching through the heap to its cached hash,
/// which the heap cannot do from inside an object. That indirection is the VM's
/// job and the table arrives with it. Until then this is correct and slow,
/// which is the right way round.
#[derive(Debug)]
pub struct ObjMap {
    pub entries: Vec<MapEntry>,
}

impl ObjMap {
    pub fn new() -> ObjMap {
        ObjMap { entries: Vec::new() }
    }
}

impl Default for ObjMap {
    fn default() -> ObjMap {
        ObjMap::new()
    }
}

/// A range, as `1..5` or `1..=5`.
#[derive(Clone, Copy, Debug)]
pub struct ObjRange {
    pub from: f64,
    pub to: f64,
    /// `..=` rather than `..`.
    pub is_inclusive: bool,
}

/// A class.
///
/// Methods are missing because a method is a function and functions arrive with
/// the compiler. What is here is what the collector needs to trace and what the
/// VM needs to answer `is` and field layout questions.
#[derive(Debug)]
pub struct ObjClass {
    /// The class's name, as a handle to an [`ObjString`].
    pub name: ObjectId,
    /// `None` only for `Object`, the root of the hierarchy.
    pub superclass: Option<ObjectId>,
    /// How many fields an instance has, **including inherited ones**.
    ///
    /// Upstream stores `-1` here to mark a foreign class, whose instances hold
    /// opaque host data rather than Wren fields. That convention is kept rather
    /// than replaced with an `Option`, so the two can be compared directly.
    pub num_fields: i32,
    /// The class holding this class's *static* methods.
    ///
    /// **This is how `System.print` works.** `System` is a class object, and
    /// calling a method on it dispatches to its metaclass, exactly as calling a
    /// method on an instance dispatches to its class. Upstream reaches the same
    /// place through every object's `classObj` header field; here only classes
    /// need it, because every other built-in's class is implied by its variant.
    ///
    /// `None` for a metaclass itself, which stops the chain.
    pub metaclass: Option<ObjectId>,
    /// Methods, indexed by symbol.
    ///
    /// **Indexed, not searched** -- a method call is an array index, which is
    /// what makes dispatch fast. The cost is that every class's table is as
    /// long as the highest symbol it responds to, so a program with many
    /// distinct method names pays for them in every class. Upstream has the
    /// same shape and the same cost.
    pub methods: Vec<Option<Method>>,
}

impl ObjClass {
    /// A class with no methods yet.
    pub fn new(name: ObjectId, superclass: Option<ObjectId>) -> ObjClass {
        ObjClass {
            name,
            superclass,
            num_fields: 0,
            metaclass: None,
            methods: Vec::new(),
        }
    }

    /// Bind a method to a symbol, growing the table as needed.
    pub fn define(&mut self, symbol: usize, method: Method) {
        if self.methods.len() <= symbol {
            self.methods.resize(symbol + 1, None);
        }
        self.methods[symbol] = Some(method);
    }

    /// The method bound to a symbol, if any.
    pub fn method(&self, symbol: usize) -> Option<Method> {
        self.methods.get(symbol).copied().flatten()
    }
}

/// What a method call actually runs.
#[derive(Clone, Copy)]
pub enum Method {
    /// Implemented in Wren: a handle to an [`ObjClosure`].
    Closure(ObjectId),
    /// Implemented in Rust.
    ///
    /// Takes the stack index of the receiver rather than a slice of arguments.
    /// A slice would mean either borrowing the VM's stack while the VM is
    /// mutably borrowed -- which does not typecheck -- or copying arguments out
    /// on every call, which on a hot dispatch path is up to 136 bytes of memcpy
    /// per call. An index costs nothing and sidesteps both.
    Primitive(Primitive),
}

impl core::fmt::Debug for Method {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Method::Closure(id) => write!(out, "Closure({})", id.raw()),
            Method::Primitive(_) => out.write_str("Primitive"),
        }
    }
}

/// A method implemented in Rust. See [`Method::Primitive`].
pub type Primitive =
    fn(&mut crate::vm::Vm, receiver: usize) -> Result<Value, crate::vm::RuntimeError>;

/// An instance of a class.
#[derive(Debug)]
pub struct ObjInstance {
    pub class: ObjectId,
    pub fields: Vec<Value>,
}

// **Layout, pinned.** These are measured, not asserted: the numbers in
// `doc/wren-rs/design.md` came from compiling for the real target and reading
// the sizes back, and these assertions are what keeps the document honest when
// a field is added. A failure here means the design note needs rewriting, not
// that the assertion needs relaxing.
#[cfg(target_pointer_width = "32")]
mod layout {
    use super::*;

    // Every slot in the heap's table is one `Object`, so this is what an
    // object costs before its contents.
    const _: () = assert!(core::mem::size_of::<Object>() == 24);

    // A free slot is `None`, and the enum's spare discriminants give `Option`
    // a niche -- so an empty slot is the same size as a full one rather than
    // one word larger. Losing this would cost a word on every slot in the
    // table, which is why it is checked rather than assumed.
    const _: () = assert!(core::mem::size_of::<Option<Object>>() == 24);

    // `Range` is the largest variant and therefore sets the slot size. Two
    // `f64`s force 8-byte alignment, which is also why shrinking it would buy
    // nothing: the next largest variants are already 16, and alignment would
    // round any tag back up to 24.
    const _: () = assert!(core::mem::size_of::<ObjRange>() == 24);
    const _: () = assert!(core::mem::size_of::<ObjString>() == 16);
    const _: () = assert!(core::mem::size_of::<ObjList>() == 12);
    const _: () = assert!(core::mem::size_of::<ObjMap>() == 12);
    // `ObjClass` is boxed, so its size no longer sets the slot size -- only
    // the pointer to it does. Checked so that the reason for boxing stays
    // visible: it is well past `Range`'s 24 bytes.
    const _: () = assert!(core::mem::size_of::<ObjClass>() > 24);
    const _: () = assert!(core::mem::size_of::<Box<ObjClass>>() == 4);
    const _: () = assert!(core::mem::size_of::<ObjInstance>() == 16);
    const _: () = assert!(core::mem::size_of::<MapEntry>() == 16);
}

/// A compiled function body.
///
/// The chunk is behind an `Rc` so that a call frame can hold onto it without
/// borrowing the heap for the duration of the call — the interpreter loop
/// mutates the stack on every instruction, and a `&Chunk` borrowed out of the
/// heap would conflict with that on the very first push.
#[derive(Debug)]
pub struct ObjFn {
    pub chunk: Rc<Chunk>,
    pub arity: usize,
    pub num_upvalues: usize,
    /// For error messages and stack traces.
    pub name: String,
    /// Added to every field index this function's body uses.
    ///
    /// **A method's fields are numbered from zero by the compiler**, which
    /// cannot know how many fields the superclass has — the superclass is a
    /// runtime expression. The offset is filled in when the method is bound to
    /// its class, at which point the superclass is known. Upstream solves the
    /// same problem by rewriting the bytecode at bind time; a field added here
    /// is the same fix without mutating a shared chunk.
    pub field_offset: usize,
    /// Where a `super` call in this body starts looking. Set at bind time,
    /// alongside [`ObjFn::field_offset`], for the same reason.
    pub super_class: Option<ObjectId>,
    /// Which module's variables this function's `LoadModuleVar` indices refer
    /// to. A function compiled in one module keeps resolving against that
    /// module wherever it is later called from.
    pub module: usize,
}

/// A function together with the variables it captured.
///
/// Every callable in Wren is one of these, even a function that captures
/// nothing — upstream does the same, so there is one kind of call rather than
/// two.
#[derive(Debug)]
pub struct ObjClosure {
    pub function: ObjectId,
    pub upvalues: Vec<ObjectId>,
}

/// One variable captured by a closure.
///
/// **Open** while the frame that owns it is still running: the variable is
/// still a live stack slot, and reads go there, so an assignment in the
/// enclosing function is visible to the closure. **Closed** once that frame
/// returns: the value is copied in here, because the stack slot is about to be
/// reused by something else.
#[derive(Clone, Copy, Debug)]
pub struct ObjUpvalue {
    /// The absolute stack slot, while open.
    pub slot: usize,
    /// The captured value, once closed.
    pub closed: Option<Value>,
}

/// A coroutine: its own value stack, its own call frames, and who to go back to.
///
/// **A fiber is a separate stack, which is the whole point.** `Fiber.yield`
/// leaves a call half-finished and returns to whoever resumed it; that is only
/// possible if the frames below the yield stay put rather than being unwound.
///
/// The running fiber's stack and frames are kept on the VM rather than in here,
/// because the interpreter touches them on every instruction and reaching
/// through the heap for each access would cost a lookup per push. They are
/// swapped back into the fiber when control moves elsewhere.
#[derive(Debug)]
pub struct ObjFiber {
    pub stack: Vec<Value>,
    pub frames: Vec<crate::vm::Frame>,
    /// The function this fiber runs, until it has been started.
    pub entry: Option<ObjectId>,
    /// Who resumed this fiber, and so where `yield` returns to.
    pub caller: Option<ObjectId>,
    /// Set when the fiber aborted, and readable through `fiber.error`.
    pub error: Value,
    /// A fiber that has run to completion. Calling one again is an error.
    pub done: bool,
    /// Whether the resumer used `try`, and so wants an error handed back
    /// rather than propagated.
    pub catching: bool,
}

impl ObjFiber {
    pub fn new(entry: ObjectId) -> ObjFiber {
        ObjFiber {
            stack: Vec::new(),
            frames: Vec::new(),
            entry: Some(entry),
            caller: None,
            error: Value::NULL,
            done: false,
            catching: false,
        }
    }
}
