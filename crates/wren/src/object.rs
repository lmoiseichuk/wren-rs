//! Heap objects, and the handles that refer to them.
//!
//! Everything that is not a number, a boolean or null lives here. Upstream
//! calls these `Obj` and reaches them through `Obj*`; this reaches them through
//! an [`ObjectId`] indexing a table the [`Heap`](crate::heap::Heap) owns. See
//! `doc/wren-rs/design.md` for why, and what it costs.

extern crate alloc;

use alloc::vec::Vec;

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
    Instance,
    List,
    Map,
    Range,
    String,
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
    Class(ObjClass),
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
            Object::Class(_) => 0,
            Object::Instance(instance) => {
                instance.fields.capacity() * core::mem::size_of::<Value>()
            }
            Object::List(list) => list.elements.capacity() * core::mem::size_of::<Value>(),
            Object::Map(map) => map.entries.capacity() * core::mem::size_of::<MapEntry>(),
            Object::Range(_) => 0,
            Object::String(string) => string.bytes.capacity(),
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
}

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
    const _: () = assert!(core::mem::size_of::<ObjClass>() == 16);
    const _: () = assert!(core::mem::size_of::<ObjInstance>() == 16);
    const _: () = assert!(core::mem::size_of::<MapEntry>() == 16);
}
