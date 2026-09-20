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

impl ObjectType {
    /// The tag a handle carries for this type.
    ///
    /// The enum's own discriminant, named rather than cast at each use so that
    /// the handle encoding and the profiler's array both point at one place.
    pub fn tag(self) -> u8 {
        self as u8
    }

    /// The type a handle's tag names, or `None` if it names nothing.
    ///
    /// A handle built by hand -- a bytecode loader, a test -- can carry any
    /// four bits, so this has to be able to say no rather than index past the
    /// end of a table list.
    pub fn from_tag(tag: u8) -> Option<ObjectType> {
        let kind = match tag {
            0 => ObjectType::Class,
            1 => ObjectType::Closure,
            2 => ObjectType::Fn,
            3 => ObjectType::Instance,
            4 => ObjectType::List,
            5 => ObjectType::Map,
            6 => ObjectType::Range,
            7 => ObjectType::String,
            8 => ObjectType::Upvalue,
            9 => ObjectType::Fiber,
            _ => return None,
        };
        Some(kind)
    }
}

// The tag has to fit in what a handle reserves for it, and the mapping above
// has to agree with the discriminants. Both are cheap to check and expensive
// to find out about later.
const _: () = assert!((ObjectType::Fiber as u32) < (1 << ObjectId::TAG_BITS));

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
    ///
    /// **Not boxed, unlike the other three composite types.** `ObjClosure` is
    /// a handle and a vector -- 16 bytes -- which fits inside the 24 that
    /// `ObjRange` already forces every slot to be, so storing it inline is
    /// free. Boxing it cost an allocation, its header, and an indirection on
    /// every call; closures are a fifth of everything this VM allocates, so
    /// that was about 24 bytes each for nothing.
    Closure(ObjClosure),
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
            Object::Class(class) => class.trace(gray),
            Object::Fn(function) => function.trace(gray),
            Object::Closure(closure) => closure.trace(gray),
            Object::Upvalue(upvalue) => upvalue.trace(gray),
            Object::Fiber(fiber) => fiber.trace(gray),
            Object::Instance(instance) => instance.trace(gray),
            Object::List(list) => list.trace(gray),
            Object::Map(map) => map.trace(gray),
            Object::Range(range) => range.trace(gray),
            Object::String(string) => string.trace(gray),
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
                    + class.methods.footprint()
            }
            Object::Instance(instance) => instance.count() * core::mem::size_of::<Value>(),
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
        ObjList {
            elements: Vec::new(),
        }
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

/// A map: open addressing with linear probing, as upstream.
///
/// **The entry table is sparse, and that is observable.** An earlier version
/// stored entries in a dense `Vec` and scanned it, which was correct for
/// lookups and wrong for iteration: `map.iterate(n)` yields *slot* indices, and
/// upstream's own tests assume the gaps a hash table leaves. A dense list
/// numbers four entries 0..3 and reports no fifth; a table of capacity eight
/// scatters them, which is what a program iterating a map actually sees.
///
/// Two sentinels live in the key, following upstream:
///
/// * **unused** -- key is `undefined`, value is `false`. A probe stops here,
///   because nothing was ever inserted past it.
/// * **tombstone** -- key is `undefined`, value is `true`. A probe continues,
///   because a key inserted after a collision may lie beyond it. Removing an
///   entry without leaving one of these would strand every key that probed
///   past it.
#[derive(Debug)]
pub struct ObjMap {
    /// Slots, and always a power of two of them, so the modulo is a mask.
    /// Empty until the first insertion.
    pub entries: Vec<MapEntry>,
    /// Live entries, which is what `count` reports. `entries.len()` is the
    /// capacity, and the two are not the same number.
    pub count: usize,
}

impl ObjMap {
    pub fn new() -> ObjMap {
        ObjMap {
            entries: Vec::new(),
            count: 0,
        }
    }

    /// Is this slot holding a real entry?
    pub fn is_live(&self, slot: usize) -> bool {
        self.entries
            .get(slot)
            .is_some_and(|entry| !entry.key.is_undefined())
    }

    /// The next live slot at or after `from`, for iteration.
    pub fn next_live(&self, from: usize) -> Option<usize> {
        (from..self.entries.len()).find(|slot| self.is_live(*slot))
    }

    /// Every live entry, in slot order.
    pub fn live(&self) -> impl Iterator<Item = &MapEntry> {
        self.entries
            .iter()
            .filter(|entry| !entry.key.is_undefined())
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
    pub from: crate::value::Num,
    pub to: crate::value::Num,
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
    /// Methods, indexed by symbol, **four bytes each**.
    ///
    /// **Indexed, not searched** -- a method call is an array index, which is
    /// what makes dispatch fast. The cost is that every class's table is as
    /// long as the highest symbol it responds to, so a program with many
    /// distinct method names pays for them in every class. Upstream has the
    /// same shape and the same cost.
    ///
    /// That cost is why an entry is a packed `u32` rather than an
    /// `Option<Method>`. A `Method` is one of a 4-byte handle or a 4-byte
    /// function pointer, but the discriminant and its alignment push
    /// `Option<Method>` to eight -- and since the tables are mostly `None`,
    /// half of every class's table was padding for a tag. Measured across
    /// `binary_trees`, the tables were 26,592 B; packed they are 13,296 B.
    ///
    /// The encoding is [`method_entry`] and its companions: `u32::MAX` for an
    /// empty slot, the top bit for "this is a primitive", and the remaining 31
    /// bits for either an [`ObjectId`] or an index into the VM's primitive
    /// table. **Closures decode with no extra load**, which is what makes this
    /// preferable to paging the symbol space -- that would have saved slightly
    /// more and put a second dependent load on every single call.
    pub methods: Methods,
    /// The class's attributes, or null when it has none the runtime can see.
    ///
    /// Built at compile time and attached when the class is created. Only
    /// attributes written `#!` survive; a plain `#` is compiled out, which is
    /// what makes attributes usable for tooling without costing a running
    /// program anything.
    pub attributes: Value,
    /// Values of the class's static fields, indexed as the compiler numbered
    /// them.
    ///
    /// **Per class, and shared by its static and instance methods alike** --
    /// `__count` means the same storage whichever kind of method touches it,
    /// and a nested class declared inside a method has its own. Upstream gets
    /// there by hoisting static fields into locals of the class body's scope
    /// and letting methods capture them as upvalues; storing them on the class
    /// is the same semantics with none of the upvalue machinery, at the cost
    /// of needing to know which class a method was defined in -- see
    /// [`ObjFn::owner_class`].
    pub static_fields: Vec<Value>,
}

impl ObjClass {
    /// A class with no methods yet.
    pub fn new(name: ObjectId, superclass: Option<ObjectId>) -> ObjClass {
        ObjClass {
            name,
            superclass,
            num_fields: 0,
            metaclass: None,
            methods: Methods::new(),
            attributes: Value::NULL,
            static_fields: Vec::new(),
        }
    }

    /// Bind a packed entry to a symbol, growing the table as needed.
    ///
    /// Takes the packed form rather than a `Method` because packing a
    /// primitive needs the VM's primitive table, which a class cannot see.
    /// [`Vm::bind_primitive`](crate::vm::Vm) and `core::define` do the packing.
    pub fn define(&mut self, symbol: usize, entry: u32) {
        // A table in flash is already complete; see [`Methods`]. Nothing
        // should reach here with one, and silently doing nothing is better
        // than a panic in a firmware -- `Heap::class_mut` is the guard.
        let Some(entries) = self.methods.as_mut() else {
            return;
        };
        if entries.len() <= symbol {
            entries.resize(symbol + 1, NO_METHOD);
        }
        entries[symbol] = entry;
    }

    /// The packed entry bound to a symbol; `NO_METHOD` when there is none.
    ///
    /// Out of range counts as absent rather than as an error: a class's table
    /// stops at the highest symbol it answers to, and every symbol past that
    /// is a method it does not have.
    pub fn method_entry(&self, symbol: usize) -> u32 {
        match self.methods.get(symbol) {
            Some(entry) => *entry,
            None => NO_METHOD,
        }
    }
}

/// A class's method table, which may live in flash rather than in the heap.
///
/// **The whole point of the borrowed arm.** A tailored core's classes are
/// decided when the image is built -- the same names, the same methods, the
/// same symbol numbers on every boot -- so there is nothing about them that
/// has to be built at start-up into RAM a small part does not have. Holding
/// the table as a `&'static [u32]` is what lets an `ObjClass` be a `static`
/// item in `.rodata` instead.
///
/// A borrowed table is immutable, and that is not a limitation but the
/// invariant: a class whose table was computed at build time is already
/// complete and already flattened, so nothing should be defining into it.
/// [`Methods::as_mut`] returns `None` rather than quietly copying, and
/// `Heap::class_mut` refuses a static class for the same reason, so the
/// mutating paths never reach one.
///
/// Costs four bytes per *dynamic* class, for the discriminant, against the
/// fifty-two a static one no longer occupies. One representation for both
/// rather than a feature-gated pair: a second code path through the dispatch
/// table is a worse thing to own than four bytes per class.
#[derive(Debug)]
pub enum Methods {
    /// Computed when the image was built; lives in flash.
    Static(&'static [u32]),
    /// Built at start-up, and still being defined into.
    Owned(Vec<u32>),
}

impl Methods {
    /// An empty table, which is what every class starts with.
    pub const fn new() -> Methods {
        Methods::Owned(Vec::new())
    }

    /// The table for writing, or `None` when it is in flash.
    pub fn as_mut(&mut self) -> Option<&mut Vec<u32>> {
        match self {
            Methods::Owned(entries) => Some(entries),
            Methods::Static(_) => None,
        }
    }

    /// What this table costs the heap: nothing at all when it is in flash.
    pub fn footprint(&self) -> usize {
        match self {
            Methods::Owned(entries) => entries.capacity() * core::mem::size_of::<u32>(),
            Methods::Static(_) => 0,
        }
    }
}

impl Default for Methods {
    fn default() -> Methods {
        Methods::new()
    }
}

impl core::ops::Deref for Methods {
    type Target = [u32];

    fn deref(&self) -> &[u32] {
        match self {
            Methods::Owned(entries) => entries,
            Methods::Static(entries) => entries,
        }
    }
}

/// A method table slot holding nothing.
///
/// `u32::MAX` rather than a separate presence bitmap: it costs one comparison
/// on a value already loaded, where a bitmap costs a second load.
pub const NO_METHOD: u32 = u32::MAX;

/// Set in a packed entry when it names a primitive rather than a closure.
const PRIMITIVE_BIT: u32 = 1 << 31;

/// Pack a Wren-implemented method.
///
/// The handle keeps its own value, so decoding is a comparison and a
/// construction rather than any arithmetic. A heap of 2^31 objects is not
/// reachable on a part this size -- it would need 48 GB of slots -- so the
/// stolen bit costs nothing real.
pub fn closure_entry(closure: ObjectId) -> u32 {
    closure.raw() & !PRIMITIVE_BIT
}

/// Pack a Rust-implemented method, by its index in the VM's primitive table.
pub fn primitive_entry(index: usize) -> u32 {
    PRIMITIVE_BIT | (index as u32 & !PRIMITIVE_BIT)
}

/// The closure a packed entry names, or `None` if it is empty or a primitive.
pub fn entry_closure(entry: u32) -> Option<ObjectId> {
    if entry == NO_METHOD || entry & PRIMITIVE_BIT != 0 {
        return None;
    }
    Some(ObjectId::new(entry))
}

/// The primitive index a packed entry names, if it names one.
pub fn entry_primitive(entry: u32) -> Option<usize> {
    if entry == NO_METHOD || entry & PRIMITIVE_BIT == 0 {
        return None;
    }
    Some((entry & !PRIMITIVE_BIT) as usize)
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
    /// Where this instance's fields begin in the heap's field arena, **biased
    /// by one**.
    ///
    /// **The fields are not here.** A `Vec` per instance is a call to the
    /// allocator per object created, with a header and a rounding of its own;
    /// across `binary_trees` that was a thousand separate blocks and about
    /// eight kilobytes of headers for twenty-four kilobytes of fields. They
    /// live end to end in one arena the heap owns instead, and an instance
    /// records where.
    ///
    /// The bias makes this `NonZeroU32`, which is the niche that keeps
    /// `Option<ObjInstance>` -- what a table slot holds -- down to twelve
    /// bytes rather than sixteen.
    at: core::num::NonZeroU32,
    /// How many fields. Wren caps a class at 255, inherited ones included.
    count: u16,
}

impl ObjInstance {
    /// An instance whose fields start at `at` in the heap's arena.
    pub fn new(class: ObjectId, at: usize, count: usize) -> ObjInstance {
        let biased = (at as u32).saturating_add(1);
        ObjInstance {
            class,
            at: core::num::NonZeroU32::new(biased).expect("biased by one"),
            count: count as u16,
        }
    }

    /// Where the fields begin in the arena.
    pub fn at(&self) -> usize {
        self.at.get() as usize - 1
    }

    /// How many fields the instance has.
    pub fn count(&self) -> usize {
        self.count as usize
    }

    /// Point the instance at a different run of the arena.
    pub fn moved_to(&mut self, at: usize, count: usize) {
        let biased = (at as u32).saturating_add(1);
        self.at = core::num::NonZeroU32::new(biased).expect("biased by one");
        self.count = count as u16;
    }
}

// **Layout, pinned.** These are measured, not asserted: the numbers in
// `doc/wren-rs/design.md` came from compiling for the real target and reading
// the sizes back, and these assertions are what keeps the document honest when
// a field is added. A failure here means the design note needs rewriting, not
// that the assertion needs relaxing.
// **The same layout with 32-bit numbers, which is a different set of numbers.**
// Kept separate rather than folded into the block above with `cfg!`, because
// the point of these assertions is that a reader can see what each build
// actually costs without reasoning about a conditional.
#[cfg(all(target_pointer_width = "32", any(feature = "f32", feature = "nofp")))]
mod layout_f32 {
    use super::*;

    // **Twenty, not twenty-four.** This is the whole memory argument for the
    // `f32` feature: a 4-byte `Value` drops the enum's alignment from 8 to 4,
    // and `ObjRange` -- which set the size at 24 with its two doubles -- is now
    // 12. The largest payload is 16, the tag makes 17, and 4-byte alignment
    // rounds that to 20 rather than to 24. Every object in the heap is 17%
    // smaller.
    const _: () = assert!(core::mem::size_of::<Object>() == 20);
    const _: () = assert!(core::mem::size_of::<Option<Object>>() == 20);

    const _: () = assert!(core::mem::size_of::<ObjRange>() == 12);
    const _: () = assert!(core::mem::size_of::<ObjString>() == 16);
    const _: () = assert!(core::mem::size_of::<ObjList>() == 12);
    const _: () = assert!(core::mem::size_of::<ObjMap>() == 16);
    // Twelve, not sixteen: the fields moved to the heap's arena, and the
    // start index is biased so `Option` has a niche to use.
    const _: () = assert!(core::mem::size_of::<ObjInstance>() == 12);
    // Held to 8 by the same `undefined` trick that holds it to 16 at full
    // width: an `Option<Value>` here would be 12 and would tie `ObjMap`.
    const _: () = assert!(core::mem::size_of::<ObjUpvalue>() == 8);

    const _: () = assert!(core::mem::size_of::<ObjClass>() > 20);
    const _: () = assert!(core::mem::size_of::<Box<ObjClass>>() == 4);
}

#[cfg(all(target_pointer_width = "32", not(feature = "f32"), not(feature = "nofp")))]
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
    // A map is a table plus its live count, which is not `entries.len()`.
    const _: () = assert!(core::mem::size_of::<ObjMap>() == 16);
    // Held to 16 by storing `undefined` for an open upvalue rather than an
    // `Option<Value>`; at 24 it would tie `Range` and, with the discriminant,
    // push every slot in the heap to 32.
    const _: () = assert!(core::mem::size_of::<ObjUpvalue>() == 16);
    // `ObjClass` is boxed, so its size no longer sets the slot size -- only
    // the pointer to it does. Checked so that the reason for boxing stays
    // visible: it is well past `Range`'s 24 bytes.
    const _: () = assert!(core::mem::size_of::<ObjClass>() > 24);
    const _: () = assert!(core::mem::size_of::<Box<ObjClass>>() == 4);
    // Twelve, not sixteen: the fields moved to the heap's arena, and the
    // start index is biased so `Option` has a niche to use.
    const _: () = assert!(core::mem::size_of::<ObjInstance>() == 12);
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
    /// The class this method was defined in, for its static fields.
    ///
    /// Not the receiver's class: an inherited method touching `__count` means
    /// the class it was *written* in, exactly as `super` does.
    pub owner_class: Option<ObjectId>,
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
    /// The absolute stack slot, while open, **stored one greater**.
    ///
    /// **So that `Option<ObjUpvalue>` costs nothing.** A slot in a table is an
    /// `Option<T>`, which is free when the payload has a spare bit pattern and
    /// a whole word when it does not. A `usize` and a `Value` have none
    /// between them, so an upvalue slot was 24 bytes for a 16-byte payload --
    /// and upvalues are 19.2% of everything this VM allocates, which made it
    /// the single largest thing that got nothing out of one table per type.
    ///
    /// Biasing by one makes the field `NonZeroU32`, which is the niche
    /// `Option` needs. Slot zero is a real stack slot, hence the bias rather
    /// than a sentinel.
    slot: core::num::NonZeroU32,
    /// The captured value once closed, and `undefined` while still open.
    ///
    /// **A sentinel rather than an `Option`.** `Option<Value>` costs sixteen
    /// bytes where a `Value` costs eight -- there is no spare bit pattern for
    /// `None` to occupy, so the discriminant needs a word of its own -- and
    /// since `ObjUpvalue` is one of the largest variants of [`Object`], those
    /// eight bytes were charged to *every* object in the heap. `undefined`
    /// already exists for exactly this: a value no Wren program can hold.
    pub closed: Value,
}

impl ObjUpvalue {
    /// Capture a stack slot. `closed` is `undefined` while it stays open.
    pub fn new(slot: usize, closed: Value) -> ObjUpvalue {
        // Saturating rather than wrapping: a stack that deep cannot exist --
        // `MAX_FRAMES` is 256 -- and a panic in the allocator would be a worse
        // answer than a wrong upvalue for a stack that does not.
        let biased = (slot as u32).saturating_add(1);
        ObjUpvalue {
            slot: core::num::NonZeroU32::new(biased).expect("biased by one"),
            closed,
        }
    }

    /// The stack slot this points at, while open.
    pub fn slot(&self) -> usize {
        self.slot.get() as usize - 1
    }

    /// Still pointing at a live stack slot.
    pub fn is_open(&self) -> bool {
        self.closed.is_undefined()
    }
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

/// What the collector needs from a type it stores.
///
/// **Per type rather than one match on an enum**, because the heap keeps one
/// table per type and a table has no enum to match on. It is also where the
/// rule lives that the note on [`Object::trace`] describes: a type that
/// forgets to report a reference here is a use-after-free in any other
/// language and a silently missing object in this one.
pub trait Trace {
    /// Push every object this one refers to onto the collector's work list.
    fn trace(&self, gray: &mut Vec<ObjectId>);

    /// Roughly what this object owns *beyond its slot*, for the collector's
    /// growth heuristic.
    ///
    /// Upstream tracks bytes allocated exactly, because it does every
    /// allocation itself. Here the `Vec`s inside an object allocate on their
    /// own, so this is whatever those have reserved -- close enough to decide
    /// when to collect, and not claimed to be more. The table adds the slot.
    fn contents_size(&self) -> usize {
        0
    }

    /// Give back memory this object over-reserved, now that it has settled.
    ///
    /// Called on everything that survives a collection, and a no-op for
    /// everything except a class: a class's method table is built by repeated
    /// `resize` and a doubling `Vec` ends up holding about half as much again
    /// as it uses. A class is settled once it has survived, because Wren
    /// cannot add a method after the body has run. A list or a string is a
    /// different matter -- shrinking one that is still being appended to buys
    /// a realloc on the next push and gives the memory straight back.
    fn settle(&mut self) {}
}

/// A boxed payload traces as the payload does.
///
/// `Class`, `Fn` and `Fiber` are 48 to 56 bytes and stay behind a pointer, so
/// their tables hold `Box<T>`; this is what lets the table's own code stay
/// generic over the two cases.
impl<T: Trace + ?Sized> Trace for alloc::boxed::Box<T> {
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        (**self).trace(gray)
    }

    fn contents_size(&self) -> usize {
        (**self).contents_size()
    }

    fn settle(&mut self) {
        (**self).settle()
    }
}

/// Push a value's handle, if it has one. Every `Trace` body needs this.
fn gray_value(value: Value, gray: &mut Vec<ObjectId>) {
    if let Some(id) = value.as_object() {
        gray.push(id);
    }
}

impl Trace for ObjClass {
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        gray.push(self.name);
        if let Some(superclass) = self.superclass {
            gray.push(superclass);
        }
        // A class's metaclass holds its static methods, and nothing else
        // refers to it. Forgetting this frees the metaclass out from under a
        // class that is still in use.
        if let Some(metaclass) = self.metaclass {
            gray.push(metaclass);
        }
        gray_value(self.attributes, gray);
        for value in &self.static_fields {
            gray_value(*value, gray);
        }
        // **And the methods themselves.** A method written in Wren is a
        // closure the method table is the only reference to -- once the class
        // definition has finished executing, the closure is gone from the
        // stack. Omitting this compiled and ran and passed every small test,
        // because nothing collected before the program was over; it appeared
        // the moment a second class pushed the heap past its first collection,
        // and presented as the *first* class's constructor silently doing
        // nothing.
        //
        // That is the failure mode this trait's note describes, and it is
        // worth having actually happened: the omission is invisible until a
        // collection runs at exactly the wrong moment.
        //
        // A primitive is a Rust function pointer with no heap object behind
        // it, so only the closures are worth following.
        for entry in self.methods.iter() {
            if let Some(closure) = entry_closure(*entry) {
                gray.push(closure);
            }
        }
    }

    fn contents_size(&self) -> usize {
        // Boxed, so the struct itself is a separate allocation.
        core::mem::size_of::<ObjClass>() + self.methods.footprint()
    }

    fn settle(&mut self) {
        if let Some(entries) = self.methods.as_mut() {
            entries.shrink_to_fit();
        }
    }
}

impl Trace for ObjFn {
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        // **A function's constants are references like any other.** A string
        // literal in a function body is a heap object reachable only from
        // here; missing it frees the literal out from under code that is about
        // to load it.
        for constant in &self.chunk.constants {
            gray_value(*constant, gray);
        }
    }

    fn contents_size(&self) -> usize {
        // The chunk is shared through an `Rc`, so charging its full size to
        // every closure over it would count the same bytes many times.
        core::mem::size_of::<ObjFn>()
    }
}

impl Trace for ObjClosure {
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        gray.push(self.function);
        for upvalue in &self.upvalues {
            gray.push(*upvalue);
        }
    }

    fn contents_size(&self) -> usize {
        self.upvalues.capacity() * core::mem::size_of::<ObjectId>()
    }
}

impl Trace for ObjUpvalue {
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        gray_value(self.closed, gray);
    }
}

impl Trace for ObjFiber {
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        // A suspended fiber's stack is live even though nothing is running on
        // it; its frames hold the only reference to the closures half way
        // through executing.
        for value in &self.stack {
            gray_value(*value, gray);
        }
        for frame in &self.frames {
            gray.push(frame.closure);
        }
        if let Some(caller) = self.caller {
            gray.push(caller);
        }
        gray_value(self.error, gray);
        if let Some(entry) = self.entry {
            gray.push(entry);
        }
    }

    fn contents_size(&self) -> usize {
        core::mem::size_of::<ObjFiber>() + self.stack.capacity() * core::mem::size_of::<Value>()
    }
}

impl Trace for ObjInstance {
    /// **Only the class.** The fields are in the heap's arena, which this
    /// cannot see, so `Heap::trace_at` follows them -- the one place where the
    /// collector needs to know more about a type than the type does.
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        gray.push(self.class);
    }

    fn contents_size(&self) -> usize {
        self.count() * core::mem::size_of::<Value>()
    }
}

impl Trace for ObjList {
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        for element in &self.elements {
            gray_value(*element, gray);
        }
    }

    fn contents_size(&self) -> usize {
        self.elements.capacity() * core::mem::size_of::<Value>()
    }
}

impl Trace for ObjMap {
    fn trace(&self, gray: &mut Vec<ObjectId>) {
        for entry in &self.entries {
            gray_value(entry.key, gray);
            gray_value(entry.value, gray);
        }
    }

    fn contents_size(&self) -> usize {
        self.entries.capacity() * core::mem::size_of::<MapEntry>()
    }
}

// Neither holds a reference: a range is two numbers, and a string owns its
// bytes outright.
impl Trace for ObjRange {
    fn trace(&self, _gray: &mut Vec<ObjectId>) {}
}

impl Trace for ObjString {
    fn trace(&self, _gray: &mut Vec<ObjectId>) {}

    fn contents_size(&self) -> usize {
        self.bytes.capacity()
    }
}
