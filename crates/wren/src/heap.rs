//! The object table, and the collector over it.
//!
//! Every heap object lives in one table the `Heap` owns, and every reference to
//! one — from a `Value`, from another object, from the VM's stack — is an
//! [`ObjectId`] indexing that table.
//!
//! # Why the policy is confined to this file
//!
//! The intended future directions for reclamation are reference counting and
//! something closer to Rust's ownership discipline. Neither is built. What is
//! built is the boundary they would need: **nothing outside this module learns
//! how an object's lifetime is decided.** The VM asks for an object by handle
//! and gets it or does not.
//!
//! `doc/wren-rs/design.md` sets out what each alternative would actually cost.
//! The short version, so that nobody reads "swappable" as "free": refcounting
//! needs to observe every copy and every death of a handle, and [`Value`] is
//! `Copy` precisely so the interpreter loop can move it without ceremony.
//! Making it non-`Copy` is the change, and it ripples. The table makes that
//! possible; it does not make it cheap.
//!
//! # Mark-sweep, as upstream
//!
//! Roots in, reachable objects marked, everything else freed, threshold moved.
//! The differences from upstream's collector are two: the marks live in a
//! bitmap beside the table rather than in a bit on each object, which keeps the
//! object itself smaller and the sweep a linear scan of words; and the work
//! list is an explicit `Vec` rather than upstream's `gray` stack on the VM.

extern crate alloc;

use alloc::vec::Vec;

use crate::object::{
    ObjClass, ObjClosure, ObjFiber, ObjFn, ObjInstance, ObjList, ObjMap, ObjRange, ObjString,
    ObjUpvalue, Object, ObjectId, ObjectType, Trace,
};
use crate::value::Value;

/// Whether reference counting is compiled in.
///
/// A `const` rather than a `#[cfg]` at each site: the barriers read better as
/// ordinary code, and a constant `false` folds them away just as completely.
/// Whether the heap keeps a young generation.
const NURSERY: bool = cfg!(feature = "nursery");

/// The smallest ceiling on floating garbage that still makes progress.
///
/// Below this, a collection would be triggered again before enough has been
/// allocated for the next one to free anything.
const MIN_HEADROOM: usize = 256;

/// How many `Value`s the *n*th field chunk holds.
///
/// **Adaptive, because one size is wrong at both ends.** Four kilobytes is a
/// good block for a program holding thousands of instances and absurd for one
/// holding two -- `method_call` allocates two instances in its whole life and
/// would pay a full chunk for them. So the first chunk is small and they grow
/// to a ceiling: 512 values, 4 KB, which is where the per-chunk bookkeeping
/// stops mattering against what the chunk holds.
///
/// The ceiling also has to leave room for one instance's fields -- 255 at the
/// very most, which is 2 KB -- in a chunk of its own.
fn field_chunk_size(index: usize) -> usize {
    match index {
        0 => 32,
        1 => 128,
        2 => 256,
        _ => 512,
    }
}

/// How an offset within a chunk is packed into an instance's start index.
///
/// The chunk number goes in the high bits and the offset in the low sixteen,
/// which is what lets chunks differ in size: an index into a flat arena would
/// have to assume they did not.
const FIELD_OFFSET_BITS: u32 = 16;
const FIELD_OFFSET_MASK: usize = (1 << FIELD_OFFSET_BITS) - 1;

/// How much the live set may grow before the next collection.
///
/// **Upstream's default is 10 MB and on these parts that means it never
/// collects until it dies.** Running the C port on an ESP32-C6 with the stock
/// configuration crashes on a null store rather than collecting — that is a
/// measured finding from step 1, not a worry. Starting small and growing is the
/// opposite default and the right one here.
const INITIAL_THRESHOLD: usize = 4 * 1024;

/// The live set is allowed to reach this multiple of its post-collection size
/// before collecting again. Upstream's `heapGrowthPercent` default is 50%,
/// i.e. the same 1.5.
///
/// **This is the dial that trades memory for time**, and on a part with 320 KB
/// it is worth knowing what it is worth. Measured on an ESP32-C6 running
/// `binary_trees`, going from 1.5 to 1.25 took peak heap from 185,156 B to
/// 160,628 B -- 13% less -- and cost 4.6% more time. The other three
/// benchmarks did not move at all, because they do not collect often enough
/// for the threshold to matter.
///
/// The default stays at upstream's 1.5 so the published comparison is like for
/// like. A firmware that would rather have the memory calls
/// [`Heap::set_growth`].
const GROWTH_NUMERATOR: usize = 3;
const GROWTH_DENOMINATOR: usize = 2;

/// What a collection did, for tests and for a `.mem` style console command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collection {
    /// Objects alive before the sweep.
    pub before: usize,
    /// Objects alive after it.
    pub after: usize,
    /// Estimated bytes held by live objects after the sweep.
    pub bytes_after: usize,
}

impl Collection {
    pub fn freed(&self) -> usize {
        self.before - self.after
    }
}

/// One type's objects: its slots, its free list and its mark bits.
///
/// **This is what "one table per type" means.** A slot holds the payload
/// itself rather than the largest variant of an enum, so a `List` costs the
/// twelve bytes a list needs instead of the twenty-four a `Range` needs. It
/// also makes the type free to ask about: it is in the handle, not in a
/// discriminant that has to be fetched.
struct Table<T> {
    slots: Vec<Option<T>>,
    /// Indices of free slots, newest first. A separate stack rather than a
    /// linked list threaded through the slots themselves: the same asymptotics,
    /// none of the aliasing that a threaded list needs `unsafe` to express.
    free: Vec<u32>,
    /// One bit per slot. Cleared at the start of each mark phase.
    marks: Vec<u64>,
    /// Slots holding an object that has not yet survived a collection.
    ///
    /// **A list and a bitmap, not a region.** The list is what a minor
    /// collection walks, so its cost is the number of young objects rather
    /// than the size of the table; the bitmap answers "is this one old?" in
    /// one test, which is what the write barrier asks on every store.
    ///
    /// Nothing moves. Promotion clears a bit and drops an index -- the
    /// migration a copying nursery pays for does not arise, because a handle
    /// is an index into a table and an object never changes slots.
    young: Vec<u32>,
    young_bits: Vec<u64>,
    /// Old slots that have had a young reference stored into them, and the
    /// bits that keep the list free of duplicates.
    remembered: Vec<u32>,
    remembered_bits: Vec<u64>,

    /// Which collection each object was born before.
    ///
    /// **Only to answer one question, and only under `profile`**: of the
    /// objects allocated since the last collection, how many are dead at the
    /// next one? That is the generational hypothesis stated as a measurement,
    /// and it is not the same as the survival rate of the whole live set --
    /// which is what was being measured before and conflates a thousand
    /// long-lived nodes with the temporaries made while walking them.
    #[cfg(feature = "profile")]
    born: Vec<u32>,
}

impl<T> Table<T> {
    fn new() -> Table<T> {
        Table {
            slots: Vec::new(),
            free: Vec::new(),
            marks: Vec::new(),
            young: Vec::new(),
            young_bits: Vec::new(),
            remembered: Vec::new(),
            remembered_bits: Vec::new(),
            #[cfg(feature = "profile")]
            born: Vec::new(),
        }
    }

    /// Record which collection an object was born before.
    #[cfg(feature = "profile")]
    fn note_birth(&mut self, index: u32, generation: u32) {
        let index = index as usize;
        if self.born.len() <= index {
            self.born.resize(index + 1, generation);
        }
        self.born[index] = generation;
    }

    /// Was this object allocated since the last collection?
    ///
    /// Distinct from [`Table::is_young`], which asks whether it is in the
    /// young *generation* -- the same idea, but one is a measurement and the
    /// other is the collector's bookkeeping.
    #[cfg(feature = "profile")]
    fn is_newborn(&self, index: u32, generation: u32) -> bool {
        self.born.get(index as usize).copied() == Some(generation)
    }

    fn allocate(&mut self, value: T) -> u32 {
        match self.free.pop() {
            Some(index) => {
                self.slots[index as usize] = Some(value);
                index
            }
            None => {
                let index = self.slots.len();
                self.slots.push(Some(value));
                if index / 64 >= self.marks.len() {
                    self.marks.push(0);
                }
                index as u32
            }
        }
    }
    fn get(&self, index: u32) -> Option<&T> {
        self.slots.get(index as usize)?.as_ref()
    }

    fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        self.slots.get_mut(index as usize)?.as_mut()
    }

    /// Is this slot's object still in the young generation?
    fn is_young(&self, index: u32) -> bool {
        let index = index as usize;
        match self.young_bits.get(index / 64) {
            Some(word) => word & (1 << (index % 64)) != 0,
            None => false,
        }
    }

    /// Put a freshly allocated slot in the young generation.
    fn make_young(&mut self, index: u32) {
        let at = index as usize;
        while self.young_bits.len() <= at / 64 {
            self.young_bits.push(0);
        }
        if self.young_bits[at / 64] & (1 << (at % 64)) == 0 {
            self.young_bits[at / 64] |= 1 << (at % 64);
            self.young.push(index);
        }
    }
    /// Note that an old slot now refers to something young.
    fn remember(&mut self, index: u32) {
        let at = index as usize;
        while self.remembered_bits.len() <= at / 64 {
            self.remembered_bits.push(0);
        }
        if self.remembered_bits[at / 64] & (1 << (at % 64)) == 0 {
            self.remembered_bits[at / 64] |= 1 << (at % 64);
            self.remembered.push(index);
        }
    }

    fn forget_all(&mut self) {
        self.remembered.clear();
        for word in &mut self.remembered_bits {
            *word = 0;
        }
    }

    /// Everything alive is old now.
    fn age_all(&mut self) {
        self.young.clear();
        for word in &mut self.young_bits {
            *word = 0;
        }
    }

    /// What one slot of this table costs, which is the whole point of having
    /// one table per type.
    fn slot_size(&self) -> usize {
        core::mem::size_of::<Option<T>>()
    }

    fn occupied(&self, index: u32) -> bool {
        matches!(self.slots.get(index as usize), Some(Some(_)))
    }

    fn clear_marks(&mut self) {
        for word in &mut self.marks {
            *word = 0;
        }
    }

    fn is_marked(&self, index: u32) -> bool {
        let index = index as usize;
        match self.marks.get(index / 64) {
            Some(word) => word & (1 << (index % 64)) != 0,
            None => false,
        }
    }

    fn set_mark(&mut self, index: u32) {
        let index = index as usize;
        if let Some(word) = self.marks.get_mut(index / 64) {
            *word |= 1 << (index % 64);
        }
    }
}

/// Run a block once for each of the heap's tables, with `$name` bound to it.
///
/// The bodies are generic over the payload type but the tables are not one
/// type, so this is a macro rather than a loop or a closure. Every place that
/// has to visit all ten -- clearing marks, sweeping, counting -- uses it, so
/// adding a type means adding it here and nowhere else.
macro_rules! each_table {
    ($heap:expr, $name:ident, $kind:ident, $body:block) => {{
        let heap = $heap;
        {
            let $name = &mut heap.classes;
            let $kind = ObjectType::Class;
            $body
        }
        {
            let $name = &mut heap.closures;
            let $kind = ObjectType::Closure;
            $body
        }
        {
            let $name = &mut heap.functions;
            let $kind = ObjectType::Fn;
            $body
        }
        {
            let $name = &mut heap.instances;
            let $kind = ObjectType::Instance;
            $body
        }
        {
            let $name = &mut heap.lists;
            let $kind = ObjectType::List;
            $body
        }
        {
            let $name = &mut heap.maps;
            let $kind = ObjectType::Map;
            $body
        }
        {
            let $name = &mut heap.ranges;
            let $kind = ObjectType::Range;
            $body
        }
        {
            let $name = &mut heap.strings;
            let $kind = ObjectType::String;
            $body
        }
        {
            let $name = &mut heap.upvalues;
            let $kind = ObjectType::Upvalue;
            $body
        }
        {
            let $name = &mut heap.fibers;
            let $kind = ObjectType::Fiber;
            $body
        }
    }};
}

/// The object tables, and the collector over them.
pub struct Heap {
    classes: Table<alloc::boxed::Box<ObjClass>>,
    closures: Table<ObjClosure>,
    functions: Table<alloc::boxed::Box<ObjFn>>,
    fibers: Table<alloc::boxed::Box<ObjFiber>>,
    instances: Table<ObjInstance>,
    lists: Table<ObjList>,
    maps: Table<ObjMap>,
    ranges: Table<ObjRange>,
    strings: Table<ObjString>,
    upvalues: Table<ObjUpvalue>,
    live: usize,
    bytes: usize,
    threshold: usize,
    /// The growth factor, as a fraction. See [`GROWTH_NUMERATOR`].
    growth: (usize, usize),
    /// An absolute ceiling on floating garbage, if one is set.
    ///
    /// **A multiple makes garbage grow with the live set; a cap does not.**
    /// At 1.5x, a program holding 200 KB is allowed 100 KB of garbage before
    /// anything is collected -- which is the wrong shape entirely for a part
    /// that has 320 KB in total. With a headroom of 16 KB the peak is the live
    /// set plus 16 KB, whatever the live set is, and a firmware can size its
    /// heap from the one number it can actually predict.
    headroom: Option<usize>,
    /// Set while a collection is not wanted — during a sequence of allocations
    /// whose intermediate results are not yet reachable from any root.
    paused: bool,
    collections: usize,
    /// Chunks of instance fields, and how far into the last one is used.
    ///
    /// **Chunks rather than one growing vector, because the heap is fixed.** A
    /// single `Vec` doubles, and doubling asks the allocator for one block
    /// twice the size of the last while still holding it -- on a part with
    /// 320 KB that is how a program with a few thousand live instances runs
    /// out of memory to hold a hundred kilobytes of fields. It did.
    ///
    /// A chunk is [`FIELD_CHUNK`] values, and growth is one chunk at a time.
    /// A run of fields never straddles two: an instance is capped at 255
    /// fields, comfortably inside a chunk, so a run that will not fit in what
    /// is left starts a new one and the remainder is wasted -- at most a
    /// couple of hundred bytes, against a doubling that could ask for a
    /// hundred kilobytes.
    ///
    /// Emptied chunks are kept rather than freed, so a program that spikes and
    /// settles reuses them instead of returning them and asking again.
    chunks: Vec<Vec<Value>>,
    /// How much of the last chunk is used.
    chunk_used: usize,
    /// Chunks compaction emptied, waiting to be filled again.
    spare_chunks: Vec<Vec<Value>>,
    ///
    /// **One allocation for all of them instead of one each.** A `Vec` inside
    /// each `ObjInstance` meant a call to the allocator per object created,
    /// with a header and a rounding of its own; measured across
    /// `binary_trees`, a thousand instances were a thousand separate blocks.
    ///
    /// A run is never moved while the program runs, so an instance's start
    /// index is stable between collections. A full collection compacts the
    /// arena and rewrites the starts, which is the one place anything moves --
    /// and it moves *contents*, not objects: a handle still names the same
    /// slot, so nothing outside this file notices.
    /// How many objects are in the young generation, kept as a running total.
    young_total: usize,
    /// What those objects cost, so a major collection is not triggered by them.
    young_bytes: usize,
    /// A reusable buffer for the short reference lists the barriers build.
    ///
    /// **Without this, counting cost an allocation per allocation.** Every
    /// object created has its references read off `trace`, and a fresh `Vec`
    /// for that is a call to the allocator for every instance, closure and
    /// list the program makes -- measured at 28% of `binary_trees`, which is
    /// more than the collector it was meant to replace.
    scratch: Vec<ObjectId>,
    #[cfg(feature = "profile")]
    profile: Profile,
}

/// What the collector did, for deciding what should replace it.
///
/// **Only built under the `profile` feature.** Every field here is a tally the
/// shipping build has no use for, and the cyclic-garbage figure costs a whole
/// second pass over the garbage at each collection.
#[cfg(feature = "profile")]
#[derive(Debug, Default, Clone)]
pub struct Profile {
    /// Objects ever allocated, by [`crate::object::ObjectType`] in
    /// declaration order.
    pub allocated: [u64; 10],
    /// How many collections ran.
    pub collections: u64,
    /// What those objects were estimated to cost, at allocation time.
    pub allocated_bytes: u64,
    /// Objects visited by the mark phase, summed over every collection. This
    /// is what tracing costs: it is proportional to the *live* set, and it is
    /// paid again at every collection whether anything died or not.
    pub marked: u64,
    /// Objects freed, summed over every collection.
    pub swept: u64,
    /// Live objects immediately after each collection, summed. Against
    /// `live_before` this gives the survival rate -- and a *high* one is the
    /// bad case, because everything that survives is marked again at every
    /// later collection for as long as it lives.
    pub survived: u64,
    /// Live objects immediately before each collection, summed.
    pub live_before: u64,
    /// Garbage a reference count would have freed the moment it died.
    pub garbage_acyclic: u64,
    /// Garbage a reference count would have leaked, because it is a cycle or
    /// is reachable only from one. This is the figure that decides whether
    /// refcounting can stand alone.
    pub garbage_cyclic: u64,
    /// Time inside `collect`, in nanoseconds.
    pub collect_nanos: u64,
    /// The high-water marks.
    pub peak_live: usize,
    pub peak_bytes: usize,
    /// Slots ever swept over. Tracing costs the live set; sweeping costs the
    /// whole table, which is a different curve and worth separating.
    pub slots_swept: u64,
    /// Objects freed by their reference count rather than by a trace.
    pub freed_promptly: u64,
    /// How many minor collections ran.
    pub minor_collections: u64,
    /// Young objects that survived one and were promoted where they stood.
    pub promoted: u64,
    /// Young objects a minor collection freed.
    pub freed_young: u64,
    /// Objects allocated, counted here so both halves of the generational
    /// question come from the same place.
    pub young_allocated: u64,
    /// Of the objects allocated since the previous collection, how many were
    /// still alive at the next one. **This is the generational hypothesis.**
    /// If it is near zero, most objects die young and a nursery reclaims
    /// almost everything for almost nothing.
    pub young_survived: u64,
    /// How many times the candidate list was confirmed against the roots.
    pub flushes: u64,
}

impl Heap {
    pub fn new() -> Heap {
        Heap {
            classes: Table::new(),
            closures: Table::new(),
            functions: Table::new(),
            fibers: Table::new(),
            instances: Table::new(),
            lists: Table::new(),
            maps: Table::new(),
            ranges: Table::new(),
            strings: Table::new(),
            upvalues: Table::new(),
            live: 0,
            bytes: 0,
            threshold: INITIAL_THRESHOLD,
            growth: (GROWTH_NUMERATOR, GROWTH_DENOMINATOR),
            headroom: None,
            paused: false,
            collections: 0,
            chunks: Vec::new(),
            chunk_used: 0,
            spare_chunks: Vec::new(),
            young_total: 0,
            young_bytes: 0,
            scratch: Vec::new(),
            #[cfg(feature = "profile")]
            profile: Profile::default(),
        }
    }

    /// Put an object in its type's table and hand back a handle to it.
    ///
    /// **This never collects.** Upstream allocates and collects in the same
    /// call, which means any allocation can free an object the caller is part
    /// way through building and is holding only in a local. Upstream handles
    /// that with a stack of temporary roots the caller has to remember to push.
    /// Here the decision is separated: allocation only allocates, and the VM
    /// calls [`should_collect`](Heap::should_collect) at a point where it knows
    /// what its roots are. A forgotten temporary root is one of the classic
    /// ways to write a collector bug, and this removes the opportunity rather
    /// than documenting it.
    ///
    /// `Object` is the constructor's argument and nothing more: it is taken
    /// apart here and the payload goes into its own table. Nothing stores one.
    pub fn allocate(&mut self, object: Object) -> ObjectId {
        #[cfg(feature = "profile")]
        {
            self.profile.allocated[object.object_type() as usize] += 1;
        }
        self.live += 1;

        // **The cost is the slot this type actually uses**, not the largest
        // slot any type uses. Charging a flat size here and a per-type size at
        // the sweep would make the running total disagree with the collector's,
        // and the growth threshold is set from one and tested against the
        // other.
        let (id, cost) = match object {
            Object::Class(class) => Self::place(&mut self.classes, ObjectType::Class, class),
            Object::Closure(closure) => {
                Self::place(&mut self.closures, ObjectType::Closure, closure)
            }
            Object::Fn(function) => Self::place(&mut self.functions, ObjectType::Fn, function),
            Object::Fiber(fiber) => Self::place(&mut self.fibers, ObjectType::Fiber, fiber),
            Object::Instance(instance) => {
                Self::place(&mut self.instances, ObjectType::Instance, instance)
            }
            Object::List(list) => Self::place(&mut self.lists, ObjectType::List, list),
            Object::Map(map) => Self::place(&mut self.maps, ObjectType::Map, map),
            Object::Range(range) => Self::place(&mut self.ranges, ObjectType::Range, range),
            Object::String(string) => Self::place(&mut self.strings, ObjectType::String, string),
            Object::Upvalue(upvalue) => {
                Self::place(&mut self.upvalues, ObjectType::Upvalue, upvalue)
            }
        };

        #[cfg(feature = "profile")]
        {
            let generation = self.collections as u32;
            let index = id.index();
            match ObjectType::from_tag(id.tag()) {
                Some(ObjectType::Class) => self.classes.note_birth(index, generation),
                Some(ObjectType::Closure) => self.closures.note_birth(index, generation),
                Some(ObjectType::Fn) => self.functions.note_birth(index, generation),
                Some(ObjectType::Fiber) => self.fibers.note_birth(index, generation),
                Some(ObjectType::Instance) => self.instances.note_birth(index, generation),
                Some(ObjectType::List) => self.lists.note_birth(index, generation),
                Some(ObjectType::Map) => self.maps.note_birth(index, generation),
                Some(ObjectType::Range) => self.ranges.note_birth(index, generation),
                Some(ObjectType::String) => self.strings.note_birth(index, generation),
                Some(ObjectType::Upvalue) => self.upvalues.note_birth(index, generation),
                None => {}
            }
            self.profile.young_allocated += 1;
        }

        if NURSERY {
            self.young_total += 1;
            self.young_bytes += cost;
            let index = id.index();
            match ObjectType::from_tag(id.tag()) {
                Some(ObjectType::Class) => self.classes.make_young(index),
                Some(ObjectType::Closure) => self.closures.make_young(index),
                Some(ObjectType::Fn) => self.functions.make_young(index),
                Some(ObjectType::Fiber) => self.fibers.make_young(index),
                Some(ObjectType::Instance) => self.instances.make_young(index),
                Some(ObjectType::List) => self.lists.make_young(index),
                Some(ObjectType::Map) => self.maps.make_young(index),
                Some(ObjectType::Range) => self.ranges.make_young(index),
                Some(ObjectType::String) => self.strings.make_young(index),
                Some(ObjectType::Upvalue) => self.upvalues.make_young(index),
                None => {}
            }
        }

        self.bytes += cost;
        #[cfg(feature = "profile")]
        {
            self.profile.allocated_bytes += cost as u64;
            let occupied = self.occupancy();
            self.profile.peak_live = self.profile.peak_live.max(occupied);
            self.profile.peak_bytes = self.profile.peak_bytes.max(self.bytes);
        }
        id
    }

    /// Put a value in its table, and say what the slot plus its contents cost.
    fn place<T: Trace>(table: &mut Table<T>, kind: ObjectType, value: T) -> (ObjectId, usize) {
        let cost = table.slot_size() + value.contents_size();
        (ObjectId::tagged(kind.tag(), table.allocate(value)), cost)
    }

    /// Look up an object of a known type.
    ///
    /// **This is the API the VM uses.** Almost every access already knows what
    /// it expects, and saying so means the heap can go straight to that type's
    /// table -- there is no enum to build and take apart, and no discriminant
    /// to fetch.
    ///
    /// A handle of the wrong type reads as `None`, exactly as a stale handle
    /// does, and the check is a comparison of four bits rather than a lookup.
    /// That is the same contract upstream's `AS_CLASS` does not offer: there,
    /// asking a string for its methods is undefined behaviour.
    pub fn class(&self, id: ObjectId) -> Option<&ObjClass> {
        match id.tag() == ObjectType::Class.tag() {
            true => self.classes.get(id.index()).map(|class| &**class),
            false => None,
        }
    }

    pub fn class_mut(&mut self, id: ObjectId) -> Option<&mut ObjClass> {
        match id.tag() == ObjectType::Class.tag() {
            true => self.classes.get_mut(id.index()).map(|class| &mut **class),
            false => None,
        }
    }

    pub fn function(&self, id: ObjectId) -> Option<&ObjFn> {
        match id.tag() == ObjectType::Fn.tag() {
            true => self.functions.get(id.index()).map(|function| &**function),
            false => None,
        }
    }

    pub fn function_mut(&mut self, id: ObjectId) -> Option<&mut ObjFn> {
        match id.tag() == ObjectType::Fn.tag() {
            true => self
                .functions
                .get_mut(id.index())
                .map(|function| &mut **function),
            false => None,
        }
    }

    pub fn fiber(&self, id: ObjectId) -> Option<&ObjFiber> {
        match id.tag() == ObjectType::Fiber.tag() {
            true => self.fibers.get(id.index()).map(|fiber| &**fiber),
            false => None,
        }
    }

    pub fn fiber_mut(&mut self, id: ObjectId) -> Option<&mut ObjFiber> {
        match id.tag() == ObjectType::Fiber.tag() {
            true => self.fibers.get_mut(id.index()).map(|fiber| &mut **fiber),
            false => None,
        }
    }

    pub fn closure(&self, id: ObjectId) -> Option<&ObjClosure> {
        match id.tag() == ObjectType::Closure.tag() {
            true => self.closures.get(id.index()),
            false => None,
        }
    }

    pub fn closure_mut(&mut self, id: ObjectId) -> Option<&mut ObjClosure> {
        match id.tag() == ObjectType::Closure.tag() {
            true => self.closures.get_mut(id.index()),
            false => None,
        }
    }

    pub fn instance(&self, id: ObjectId) -> Option<&ObjInstance> {
        match id.tag() == ObjectType::Instance.tag() {
            true => self.instances.get(id.index()),
            false => None,
        }
    }

    pub fn instance_mut(&mut self, id: ObjectId) -> Option<&mut ObjInstance> {
        match id.tag() == ObjectType::Instance.tag() {
            true => self.instances.get_mut(id.index()),
            false => None,
        }
    }

    pub fn list(&self, id: ObjectId) -> Option<&ObjList> {
        match id.tag() == ObjectType::List.tag() {
            true => self.lists.get(id.index()),
            false => None,
        }
    }

    pub fn list_mut(&mut self, id: ObjectId) -> Option<&mut ObjList> {
        match id.tag() == ObjectType::List.tag() {
            true => self.lists.get_mut(id.index()),
            false => None,
        }
    }

    pub fn map(&self, id: ObjectId) -> Option<&ObjMap> {
        match id.tag() == ObjectType::Map.tag() {
            true => self.maps.get(id.index()),
            false => None,
        }
    }

    pub fn map_mut(&mut self, id: ObjectId) -> Option<&mut ObjMap> {
        match id.tag() == ObjectType::Map.tag() {
            true => self.maps.get_mut(id.index()),
            false => None,
        }
    }

    pub fn range(&self, id: ObjectId) -> Option<&ObjRange> {
        match id.tag() == ObjectType::Range.tag() {
            true => self.ranges.get(id.index()),
            false => None,
        }
    }

    pub fn string(&self, id: ObjectId) -> Option<&ObjString> {
        match id.tag() == ObjectType::String.tag() {
            true => self.strings.get(id.index()),
            false => None,
        }
    }

    pub fn string_mut(&mut self, id: ObjectId) -> Option<&mut ObjString> {
        match id.tag() == ObjectType::String.tag() {
            true => self.strings.get_mut(id.index()),
            false => None,
        }
    }

    pub fn upvalue(&self, id: ObjectId) -> Option<&ObjUpvalue> {
        match id.tag() == ObjectType::Upvalue.tag() {
            true => self.upvalues.get(id.index()),
            false => None,
        }
    }

    pub fn upvalue_mut(&mut self, id: ObjectId) -> Option<&mut ObjUpvalue> {
        match id.tag() == ObjectType::Upvalue.tag() {
            true => self.upvalues.get_mut(id.index()),
            false => None,
        }
    }
    /// Create an instance whose fields are `fields`.
    ///
    /// The only way to make one: an `ObjInstance` records where its fields are
    /// in the arena, so it cannot be built without the heap that owns it.
    pub fn new_instance(&mut self, class: ObjectId, fields: &[Value]) -> ObjectId {
        let at = self.place_fields(fields);
        self.allocate(Object::Instance(ObjInstance::new(class, at, fields.len())))
    }

    /// Put a run of fields in a chunk and say where it went.
    fn place_fields(&mut self, fields: &[Value]) -> usize {
        let room = match self.chunks.last() {
            Some(chunk) => chunk.len().saturating_sub(self.chunk_used),
            None => 0,
        };
        // **`is_empty` as well as `room`**, because an instance with no fields
        // asks for nothing and would otherwise index a chunk that is not there.
        // A class with no fields is ordinary Wren.
        if self.chunks.is_empty() || room < fields.len() {
            let wanted = field_chunk_size(self.chunks.len()).max(fields.len());
            let chunk = match self.spare_chunks.pop() {
                Some(chunk) if chunk.len() >= wanted => chunk,
                other => {
                    if let Some(chunk) = other {
                        self.spare_chunks.push(chunk);
                    }
                    alloc::vec![Value::NULL; wanted]
                }
            };
            self.chunks.push(chunk);
            self.chunk_used = 0;
        }
        let chunk = self.chunks.len() - 1;
        let offset = self.chunk_used;
        self.chunks[chunk][offset..offset + fields.len()].copy_from_slice(fields);
        self.chunk_used += fields.len();
        (chunk << FIELD_OFFSET_BITS) | offset
    }

    /// Split a start index into the chunk it names and the offset within it.
    fn field_place(at: usize) -> (usize, usize) {
        (at >> FIELD_OFFSET_BITS, at & FIELD_OFFSET_MASK)
    }

    /// An instance's fields.
    ///
    /// One run never crosses a chunk boundary, which is what lets this hand
    /// back a slice rather than an iterator.
    pub fn instance_fields(&self, id: ObjectId) -> &[Value] {
        let Some(instance) = self.instance(id) else {
            return &[];
        };
        let (chunk, offset) = Self::field_place(instance.at());
        match self.chunks.get(chunk) {
            Some(chunk) => match chunk.get(offset..offset + instance.count()) {
                Some(fields) => fields,
                None => &[],
            },
            None => &[],
        }
    }

    /// One field, or `None` if the instance does not have that many.
    pub fn instance_field(&self, id: ObjectId, index: usize) -> Option<Value> {
        self.instance_fields(id).get(index).copied()
    }

    /// Store one field, growing the instance if it is short.
    ///
    /// **Growing means relocating**, because the arena is packed: the run
    /// moves to the end and the old one becomes a hole that the next
    /// compaction reclaims. It is a path that should not run -- an instance is
    /// created with the field count its class declares -- but the field index
    /// comes from bytecode, so it is handled rather than trusted.
    pub fn set_instance_field(&mut self, id: ObjectId, index: usize, value: Value) {
        let Some(instance) = self.instance(id) else {
            return;
        };
        let (at, count) = (instance.at(), instance.count());
        if index < count {
            let (chunk, offset) = Self::field_place(at);
            if let Some(slot) = self
                .chunks
                .get_mut(chunk)
                .and_then(|c| c.get_mut(offset + index))
            {
                *slot = value;
            }
            self.wrote(id, value);
            return;
        }

        // Growing means relocating, because a run is packed against its
        // neighbour. The old run becomes a hole the next compaction closes.
        let wanted = index + 1;
        let mut grown = self.instance_fields(id).to_vec();
        grown.resize(wanted, Value::NULL);
        grown[index] = value;
        let moved = self.place_fields(&grown);
        if let Some(instance) = self.instance_mut(id) {
            instance.moved_to(moved, wanted);
        }
        self.bytes += (wanted - count) * core::mem::size_of::<Value>();
        self.wrote(id, value);
    }

    /// A heap object's field now holds `new` where it held `old`.
    ///
    /// **The whole of the write barrier.** If an *old* object has just been
    /// made to point at a *young* one, a minor collection would not otherwise
    /// find the young object -- it does not scan old objects -- so the old one
    /// joins the remembered set.
    ///
    /// Everything else is free: a store into a young object needs nothing,
    /// because a young object is scanned anyway, and a store of anything that
    /// is not a young handle needs nothing either. That is the difference
    /// between this and a reference count, which has to act on every store
    /// whatever is being stored.
    pub fn wrote(&mut self, target: ObjectId, new: Value) {
        if NURSERY {
            self.note_old_to_young(target, new);
        }
    }

    /// Record an old object that has been pointed at something young.
    fn note_old_to_young(&mut self, target: ObjectId, new: Value) {
        let Some(young) = new.as_object() else {
            return;
        };
        if !self.is_young(young) || self.is_young(target) {
            return;
        }
        let index = target.index();
        match ObjectType::from_tag(target.tag()) {
            Some(ObjectType::Class) => self.classes.remember(index),
            Some(ObjectType::Closure) => self.closures.remember(index),
            Some(ObjectType::Fn) => self.functions.remember(index),
            Some(ObjectType::Fiber) => self.fibers.remember(index),
            Some(ObjectType::Instance) => self.instances.remember(index),
            Some(ObjectType::List) => self.lists.remember(index),
            Some(ObjectType::Map) => self.maps.remember(index),
            Some(ObjectType::Range) => self.ranges.remember(index),
            Some(ObjectType::String) => self.strings.remember(index),
            Some(ObjectType::Upvalue) => self.upvalues.remember(index),
            None => {}
        }
    }

    /// Has this object yet to survive a collection?
    pub fn is_young(&self, id: ObjectId) -> bool {
        let index = id.index();
        match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => self.classes.is_young(index),
            Some(ObjectType::Closure) => self.closures.is_young(index),
            Some(ObjectType::Fn) => self.functions.is_young(index),
            Some(ObjectType::Fiber) => self.fibers.is_young(index),
            Some(ObjectType::Instance) => self.instances.is_young(index),
            Some(ObjectType::List) => self.lists.is_young(index),
            Some(ObjectType::Map) => self.maps.is_young(index),
            Some(ObjectType::Range) => self.ranges.is_young(index),
            Some(ObjectType::String) => self.strings.is_young(index),
            Some(ObjectType::Upvalue) => self.upvalues.is_young(index),
            None => false,
        }
    }

    /// How many objects are in the young generation.
    ///
    /// **One load, because the interpreter asks between every instruction.**
    /// Summing ten vector lengths here instead cost 13-15% of every benchmark,
    /// including the ones that never allocate enough to collect at all -- the
    /// check was more expensive than the collection it was deciding about.
    pub fn young(&self) -> usize {
        self.young_total
    }
    /// What one object costs, slot and contents.
    fn size_at(&self, id: ObjectId) -> usize {
        let index = id.index();
        macro_rules! size {
            ($table:expr) => {
                match $table.get(index) {
                    Some(value) => $table.slot_size() + value.contents_size(),
                    None => 0,
                }
            };
        }
        match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => size!(self.classes),
            Some(ObjectType::Closure) => size!(self.closures),
            Some(ObjectType::Fn) => size!(self.functions),
            Some(ObjectType::Fiber) => size!(self.fibers),
            Some(ObjectType::Instance) => size!(self.instances),
            Some(ObjectType::List) => size!(self.lists),
            Some(ObjectType::Map) => size!(self.maps),
            Some(ObjectType::Range) => size!(self.ranges),
            Some(ObjectType::String) => size!(self.strings),
            Some(ObjectType::Upvalue) => size!(self.upvalues),
            None => 0,
        }
    }
    /// What kind of object a handle names, **from the handle alone**.
    ///
    /// No table is touched: the type is four bits of the handle. That is the
    /// whole dispatch win of one table per type, and it is why this exists
    /// beside [`type_of`](Heap::type_of) rather than being folded into it --
    /// `class_of` runs on every method call and does not need to know whether
    /// the object is still there, only what kind of thing it is.
    ///
    /// Says nothing about whether the object is live. A handle that has been
    /// swept still names the type it used to be.
    pub fn kind_of(&self, id: ObjectId) -> Option<ObjectType> {
        ObjectType::from_tag(id.tag())
    }

    /// What kind of object a handle refers to, or `None` if it refers to
    /// nothing any more.
    ///
    /// The checked form: it reads the type's table to see whether the slot is
    /// still occupied. Diagnostics and printing want this; the dispatch path
    /// wants [`kind_of`](Heap::kind_of).
    pub fn type_of(&self, id: ObjectId) -> Option<ObjectType> {
        let kind = ObjectType::from_tag(id.tag())?;
        match self.is_live(id) {
            true => Some(kind),
            false => None,
        }
    }

    /// Is there still an object under this handle?
    fn is_live(&self, id: ObjectId) -> bool {
        let index = id.index();
        match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => self.classes.occupied(index),
            Some(ObjectType::Closure) => self.closures.occupied(index),
            Some(ObjectType::Fn) => self.functions.occupied(index),
            Some(ObjectType::Fiber) => self.fibers.occupied(index),
            Some(ObjectType::Instance) => self.instances.occupied(index),
            Some(ObjectType::List) => self.lists.occupied(index),
            Some(ObjectType::Map) => self.maps.occupied(index),
            Some(ObjectType::Range) => self.ranges.occupied(index),
            Some(ObjectType::String) => self.strings.occupied(index),
            Some(ObjectType::Upvalue) => self.upvalues.occupied(index),
            None => false,
        }
    }

    /// Close the holes a sweep left in the field arena.
    ///
    /// **The one place anything moves, and it moves contents rather than
    /// objects.** A dead instance leaves its run of fields behind as a gap;
    /// without this the arena only grows. Every live instance's fields are
    /// copied down in order and its start index rewritten -- which is
    /// invisible outside this file, because a handle still names the same
    /// slot in the same table.
    ///
    /// Only at a full collection. A minor one leaves its holes for the next
    /// major to close, because walking every instance is a cost proportional
    /// to the live set and that is exactly what a minor collection exists not
    /// to pay.
    fn compact_fields(&mut self) {
        // **In place, in source order.** Compacting moves every run to an
        // address at or below where it was, so copying them in increasing
        // order of where they *are* can never overwrite one that has not been
        // copied yet. Building a fresh set of chunks instead would mean
        // holding two copies of the arena at once, which on a fixed heap is
        // the spike this whole structure exists to avoid.
        let mut runs: Vec<(u32, u32, u32)> = Vec::new();
        for index in 0..self.instances.slots.len() {
            let index = index as u32;
            if let Some(instance) = self.instances.get(index) {
                runs.push((instance.at() as u32, instance.count() as u32, index));
            }
        }
        runs.sort_unstable();

        let mut write_chunk = 0usize;
        let mut write_offset = 0usize;
        let mut run: Vec<Value> = Vec::new();
        for (at, count, index) in runs {
            let count = count as usize;
            let (from_chunk, from_offset) = Self::field_place(at as usize);

            // A run never straddles a chunk, so a destination that would have
            // to moves to the next one and the remainder of this one is lost.
            // At most a couple of hundred bytes, against a doubling arena that
            // could ask the allocator for a hundred kilobytes.
            while write_chunk < self.chunks.len()
                && write_offset + count > self.chunks[write_chunk].len()
            {
                write_chunk += 1;
                write_offset = 0;
            }
            if write_chunk >= self.chunks.len() {
                break;
            }

            if (write_chunk, write_offset) != (from_chunk, from_offset) {
                run.clear();
                for step in 0..count {
                    let value = self.chunks[from_chunk]
                        .get(from_offset + step)
                        .copied()
                        .unwrap_or(Value::NULL);
                    run.push(value);
                }
                self.chunks[write_chunk][write_offset..write_offset + count].copy_from_slice(&run);
                if let Some(instance) = self.instances.get_mut(index) {
                    instance.moved_to((write_chunk << FIELD_OFFSET_BITS) | write_offset, count);
                }
            }
            write_offset += count;
        }

        // **Emptied chunks are kept, not freed.** A program that spikes and
        // settles refills them; handing them back only to ask again is how a
        // fixed heap fragments. But keeping one for every chunk in use doubles
        // the arena, which is worse than the fragmentation -- so one is kept,
        // and the rest go back.
        self.chunk_used = write_offset;
        let needed = match self.chunks.is_empty() {
            true => 0,
            false => write_chunk + 1,
        };
        while self.chunks.len() > needed {
            if let Some(chunk) = self.chunks.pop() {
                self.spare_chunks.push(chunk);
            }
        }
        self.spare_chunks.truncate(1);
    }

    /// Empty a slot and put it back on its table's free list.
    ///
    /// Dropping the payload releases whatever its `Vec`s held.
    fn discard(&mut self, id: ObjectId) {
        let index = id.index();
        macro_rules! discard {
            ($table:expr) => {{
                if $table
                    .slots
                    .get(index as usize)
                    .is_some_and(Option::is_some)
                {
                    $table.slots[index as usize] = None;
                    $table.free.push(index);
                }
            }};
        }
        match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => discard!(self.classes),
            Some(ObjectType::Closure) => discard!(self.closures),
            Some(ObjectType::Fn) => discard!(self.functions),
            Some(ObjectType::Fiber) => discard!(self.fibers),
            Some(ObjectType::Instance) => discard!(self.instances),
            Some(ObjectType::List) => discard!(self.lists),
            Some(ObjectType::Map) => discard!(self.maps),
            Some(ObjectType::Range) => discard!(self.ranges),
            Some(ObjectType::String) => discard!(self.strings),
            Some(ObjectType::Upvalue) => discard!(self.upvalues),
            None => {}
        }
    }

    /// Push everything the object under `id` refers to onto the work list.
    fn trace_at(&self, id: ObjectId, gray: &mut Vec<ObjectId>) {
        let index = id.index();
        match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => trace_slot(self.classes.get(index), gray),
            Some(ObjectType::Closure) => trace_slot(self.closures.get(index), gray),
            Some(ObjectType::Fn) => trace_slot(self.functions.get(index), gray),
            Some(ObjectType::Fiber) => trace_slot(self.fibers.get(index), gray),
            Some(ObjectType::Instance) => {
                trace_slot(self.instances.get(index), gray);
                // The fields are in the arena, which `ObjInstance::trace`
                // cannot reach. This is the one type the collector knows more
                // about than the type knows about itself.
                for field in self.instance_fields(id) {
                    if let Some(referent) = field.as_object() {
                        gray.push(referent);
                    }
                }
            }
            Some(ObjectType::List) => trace_slot(self.lists.get(index), gray),
            Some(ObjectType::Map) => trace_slot(self.maps.get(index), gray),
            Some(ObjectType::Range) => trace_slot(self.ranges.get(index), gray),
            Some(ObjectType::String) => trace_slot(self.strings.get(index), gray),
            Some(ObjectType::Upvalue) => trace_slot(self.upvalues.get(index), gray),
            None => {}
        }
    }

    /// Mark the object under `id`; `false` if it was already marked or gone.
    fn mark_at(&mut self, id: ObjectId) -> bool {
        let index = id.index();
        macro_rules! mark {
            ($table:expr) => {{
                if !$table.occupied(index) || $table.is_marked(index) {
                    false
                } else {
                    $table.set_mark(index);
                    true
                }
            }};
        }
        match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => mark!(self.classes),
            Some(ObjectType::Closure) => mark!(self.closures),
            Some(ObjectType::Fn) => mark!(self.functions),
            Some(ObjectType::Fiber) => mark!(self.fibers),
            Some(ObjectType::Instance) => mark!(self.instances),
            Some(ObjectType::List) => mark!(self.lists),
            Some(ObjectType::Map) => mark!(self.maps),
            Some(ObjectType::Range) => mark!(self.ranges),
            Some(ObjectType::String) => mark!(self.strings),
            Some(ObjectType::Upvalue) => mark!(self.upvalues),
            None => false,
        }
    }

    /// How many slots are occupied, across every table.
    #[cfg(feature = "profile")]
    fn occupancy(&self) -> usize {
        let mut total = 0;
        total += self.classes.slots.len() - self.classes.free.len();
        total += self.closures.slots.len() - self.closures.free.len();
        total += self.functions.slots.len() - self.functions.free.len();
        total += self.fibers.slots.len() - self.fibers.free.len();
        total += self.instances.slots.len() - self.instances.free.len();
        total += self.lists.slots.len() - self.lists.free.len();
        total += self.maps.slots.len() - self.maps.free.len();
        total += self.ranges.slots.len() - self.ranges.free.len();
        total += self.strings.slots.len() - self.strings.free.len();
        total += self.upvalues.slots.len() - self.upvalues.free.len();
        total
    }

    /// Every live handle, table by table.
    ///
    /// **Not a traversal the running VM does.** This exists for the one-off
    /// passes that have to see the whole heap -- flattening the class hierarchy
    /// after the core library is installed, and diagnostics. The collector does
    /// not use it: it walks from roots, which is the point of having roots.
    pub fn ids(&self) -> Vec<ObjectId> {
        let mut out = Vec::new();
        macro_rules! collect_ids {
            ($table:expr, $kind:expr) => {
                for (index, slot) in $table.slots.iter().enumerate() {
                    if slot.is_some() {
                        out.push(ObjectId::tagged($kind.tag(), index as u32));
                    }
                }
            };
        }
        collect_ids!(self.classes, ObjectType::Class);
        collect_ids!(self.closures, ObjectType::Closure);
        collect_ids!(self.functions, ObjectType::Fn);
        collect_ids!(self.instances, ObjectType::Instance);
        collect_ids!(self.lists, ObjectType::List);
        collect_ids!(self.maps, ObjectType::Map);
        collect_ids!(self.ranges, ObjectType::Range);
        collect_ids!(self.strings, ObjectType::String);
        collect_ids!(self.upvalues, ObjectType::Upvalue);
        collect_ids!(self.fibers, ObjectType::Fiber);
        out
    }

    /// Choose how much the live set may grow before collecting again.
    ///
    /// `set_growth(5, 4)` collects at 1.25x rather than the default 1.5x:
    /// less floating garbage held, more time in the collector. A denominator
    /// of zero, or a factor below 1, would mean collecting forever, so both
    /// are clamped rather than trusted -- this is a knob a firmware sets once
    /// at start-up, and a typo in it should not be an infinite loop.
    pub fn set_growth(&mut self, numerator: usize, denominator: usize) {
        let denominator = denominator.max(1);
        let numerator = numerator.max(denominator + 1);
        self.growth = (numerator, denominator);
    }

    /// Collect once the live set has grown by this many bytes, rather than by
    /// a multiple of itself.
    ///
    /// **This is the setting for a fixed heap**, and it is the one knob on
    /// this type that a firmware is expected to touch. The growth factor is
    /// upstream's and it is a ratio, so the garbage a program may accumulate
    /// scales with how much it is holding -- exactly backwards for a part
    /// where the total is a constant. A headroom makes peak memory **the live
    /// set plus this many bytes**, which is a number that can be budgeted
    /// against before the program is written.
    ///
    /// **It means exactly what it says: [`INITIAL_THRESHOLD`] does not apply.**
    /// That floor exists so a program with a handful of objects does not
    /// collect on its third allocation, and it is 4 KB -- which is half the
    /// RAM of a CH32V006 and would quietly swallow any ceiling smaller than
    /// itself. A caller asking for 1 KB gets 1 KB.
    ///
    /// **Small is cheaper than it looks on a small part.** Collection costs
    /// the live set, so the parts that most want a tight ceiling are the ones
    /// where tightening it costs least: a program holding two hundred objects
    /// pays almost nothing to collect them often, where `binary_trees` holding
    /// a thousand-node tree pays for every sweep.
    ///
    /// `None` restores the ratio, which is what the published benchmarks use
    /// so that they stay comparable with the C port.
    pub fn set_headroom(&mut self, bytes: Option<usize>) {
        // A ceiling of nothing would mean collecting after every allocation,
        // freeing nothing and collecting again -- a livelock rather than a
        // tight bound. Clamped rather than trusted, for the same reason
        // `set_growth` clamps: this is set once at start-up and a typo in it
        // should not be an infinite loop.
        self.headroom = bytes.map(|bytes| bytes.max(MIN_HEADROOM));
    }

    /// The ceiling on floating garbage, if one is set.
    pub fn headroom(&self) -> Option<usize> {
        self.headroom
    }

    /// The growth factor in force, as `(numerator, denominator)`.
    pub fn growth(&self) -> (usize, usize) {
        self.growth
    }

    /// Where the next collection is due, in estimated live bytes.
    pub fn threshold(&self) -> usize {
        self.threshold
    }

    /// What the collector has been doing. See [`Profile`].
    #[cfg(feature = "profile")]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Is the live set big enough that collecting is worth it?
    ///
    /// The VM asks this between instructions, where its roots are well defined,
    /// rather than having the answer forced on it mid-allocation.
    pub fn should_collect(&self) -> bool {
        // **A major collection is about the old generation.** Counting the
        // nursery towards its threshold means the major always fires first --
        // the heap passes 1.5x its live size in a few hundred allocations --
        // and the nursery never fills, which is exactly what happened: 139
        // major collections and not one minor.
        !self.paused && self.bytes.saturating_sub(self.young_bytes) >= self.threshold
    }

    /// Suspend collection while a multi-step construction is in flight.
    ///
    /// The honest use: building a list of freshly allocated strings, where the
    /// strings are reachable only from a Rust local until the list exists. The
    /// alternative is upstream's temporary-root stack, and this is the smaller
    /// mechanism.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Check the invariant a minor collection depends on.
    ///
    /// **An old object may only point at a young one if the barrier recorded
    /// it.** A minor collection does not scan old objects, so an unrecorded
    /// old-to-young reference means a live young object goes unmarked and is
    /// freed while something still points at it -- the same class of failure a
    /// missing `retain` was, and just as invisible until a collection happens
    /// at exactly the wrong moment.
    ///
    /// Fibers are exempt: their stacks are moved on and off the VM rather than
    /// stored through the barrier, so a minor collection scans every old fiber
    /// unconditionally.
    ///
    /// Returns `(old object, the young thing it points at)` for each breach.
    pub fn verify_remembered(&self) -> Vec<(ObjectId, ObjectId)> {
        let mut breaches = Vec::new();
        let mut referents: Vec<ObjectId> = Vec::new();
        for id in self.ids() {
            if self.is_young(id) || ObjectType::from_tag(id.tag()) == Some(ObjectType::Fiber) {
                continue;
            }
            referents.clear();
            self.trace_at(id, &mut referents);
            for target in &referents {
                if self.is_young(*target) && !self.is_remembered(id) {
                    breaches.push((id, *target));
                }
            }
        }
        breaches
    }

    /// Whether a slot carries a mark, for the garbage analysis below.
    #[cfg(feature = "profile")]
    fn is_marked_at(&self, id: ObjectId) -> bool {
        let index = id.index();
        match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => self.classes.is_marked(index),
            Some(ObjectType::Closure) => self.closures.is_marked(index),
            Some(ObjectType::Fn) => self.functions.is_marked(index),
            Some(ObjectType::Fiber) => self.fibers.is_marked(index),
            Some(ObjectType::Instance) => self.instances.is_marked(index),
            Some(ObjectType::List) => self.lists.is_marked(index),
            Some(ObjectType::Map) => self.maps.is_marked(index),
            Some(ObjectType::Range) => self.ranges.is_marked(index),
            Some(ObjectType::String) => self.strings.is_marked(index),
            Some(ObjectType::Upvalue) => self.upvalues.is_marked(index),
            None => false,
        }
    }

    /// Is this old object in the remembered set?
    fn is_remembered(&self, id: ObjectId) -> bool {
        let index = id.index() as usize;
        let bits = match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => &self.classes.remembered_bits,
            Some(ObjectType::Closure) => &self.closures.remembered_bits,
            Some(ObjectType::Fn) => &self.functions.remembered_bits,
            Some(ObjectType::Fiber) => &self.fibers.remembered_bits,
            Some(ObjectType::Instance) => &self.instances.remembered_bits,
            Some(ObjectType::List) => &self.lists.remembered_bits,
            Some(ObjectType::Map) => &self.maps.remembered_bits,
            Some(ObjectType::Range) => &self.ranges.remembered_bits,
            Some(ObjectType::String) => &self.strings.remembered_bits,
            Some(ObjectType::Upvalue) => &self.upvalues.remembered_bits,
            None => return false,
        };
        match bits.get(index / 64) {
            Some(word) => word & (1 << (index % 64)) != 0,
            None => false,
        }
    }

    /// Whether this build keeps a young generation.
    pub fn nursery() -> bool {
        NURSERY
    }

    /// Collect the young generation only.
    ///
    /// **Nothing moves and nothing is copied.** A generation here is a bitmap
    /// over slots, not a region of memory, so a survivor is promoted by
    /// clearing its bit and an object never changes address. That is what
    /// makes this affordable: the migration a copying nursery pays for does
    /// not exist, because a handle is an index and the index does not change.
    ///
    /// The trace starts from the roots and from the objects the write barrier
    /// remembered, and **it does not scan old objects**. That is sound because
    /// of the invariant the barrier maintains: an old object can only come to
    /// point at a young one through a store, and every such store is recorded.
    /// References it made while it was itself young point at objects that were
    /// promoted alongside it.
    ///
    /// Cost is the young generation and the remembered set, not the live set.
    /// 84% of what this VM allocates is dead by the next collection, so most
    /// of that work is a bitmap test that says "free it".
    pub fn collect_minor(&mut self, roots: &[ObjectId]) -> Collection {
        #[cfg(feature = "profile")]
        let started = std::time::Instant::now();
        let before = self.live;

        each_table!(&mut *self, table, _kind, {
            table.clear_marks();
        });

        let mut gray = core::mem::take(&mut self.scratch);
        gray.clear();
        gray.extend_from_slice(roots);

        // The old objects that have to be looked at anyway: those written to
        // since the last minor collection, and every old fiber -- a fiber's
        // stack is moved on and off the VM rather than stored through the
        // barrier, so it cannot be tracked the same way.
        let mut scan: Vec<ObjectId> = Vec::new();
        each_table!(&mut *self, table, kind, {
            for index in &table.remembered {
                scan.push(ObjectId::tagged(kind.tag(), *index));
            }
        });
        for index in 0..self.fibers.slots.len() {
            let index = index as u32;
            if self.fibers.occupied(index) && !self.fibers.is_young(index) {
                scan.push(ObjectId::tagged(ObjectType::Fiber.tag(), index));
            }
        }
        for id in &scan {
            self.trace_at(*id, &mut gray);
        }

        let mut referents: Vec<ObjectId> = Vec::new();
        while let Some(id) = gray.pop() {
            // An old object is not scanned: whatever young thing it points at
            // is in the remembered set, and it cannot itself be collected here.
            if !self.is_young(id) || !self.mark_at(id) {
                continue;
            }
            #[cfg(feature = "profile")]
            {
                self.profile.marked += 1;
            }
            referents.clear();
            self.trace_at(id, &mut referents);
            gray.extend(referents.iter().copied());
        }
        gray.clear();
        self.scratch = gray;

        // Sweep the young generation, and nothing else. Survivors are promoted
        // where they stand.
        let mut dead: Vec<ObjectId> = Vec::new();
        let mut promoted = 0usize;
        each_table!(&mut *self, table, kind, {
            for index in core::mem::take(&mut table.young) {
                match table.is_marked(index) {
                    true => promoted += 1,
                    false => dead.push(ObjectId::tagged(kind.tag(), index)),
                }
            }
        });

        let mut live = self.live;
        for id in &dead {
            self.bytes = self.bytes.saturating_sub(self.size_at(*id));
            live = live.saturating_sub(1);
            self.discard(*id);
        }
        self.live = live;

        // Everything that survived is old, and nothing old points at anything
        // young any more.
        each_table!(&mut *self, table, _kind, {
            table.age_all();
            table.forget_all();
        });
        self.young_total = 0;
        self.young_bytes = 0;

        #[cfg(feature = "profile")]
        {
            self.profile.minor_collections += 1;
            self.profile.promoted += promoted as u64;
            self.profile.freed_young += dead.len() as u64;
            self.profile.collect_nanos += started.elapsed().as_nanos() as u64;
        }
        #[cfg(not(feature = "profile"))]
        let _ = promoted;

        Collection {
            before,
            after: live,
            bytes_after: self.bytes,
        }
    }

    /// Mark everything reachable from `roots`, free the rest, move the
    /// threshold.
    ///
    /// The roots are whatever the caller says they are — the VM's stack, its
    /// module variables, the fibers it is holding. **An object reachable only
    /// from a Rust local that is not in `roots` will be freed**, which is the
    /// same contract upstream has and the reason [`pause`](Heap::pause) exists.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = Value>) -> Collection {
        #[cfg(feature = "profile")]
        let started = std::time::Instant::now();
        let before = self.live;

        each_table!(&mut *self, table, _kind, {
            table.clear_marks();
        });

        // Mark. A `Vec` as the work list rather than recursion: a deep object
        // graph would otherwise be a stack overflow, and on a part with 8 KB of
        // RAM the stack is the scarcest thing there is.
        let mut gray: Vec<ObjectId> = Vec::new();
        for root in roots {
            if let Some(id) = root.as_object() {
                gray.push(id);
            }
        }

        let mut referents: Vec<ObjectId> = Vec::new();
        while let Some(id) = gray.pop() {
            if !self.mark_at(id) {
                continue;
            }
            #[cfg(feature = "profile")]
            {
                self.profile.marked += 1;
            }
            referents.clear();
            self.trace_at(id, &mut referents);
            gray.extend(referents.iter().copied());
        }

        #[cfg(feature = "profile")]
        self.profile_garbage();

        // Sweep, one table at a time. Each knows its own slot size, which is
        // the whole point: a `List` slot is twelve bytes where a `Range` slot
        // is twenty-four, and neither pays for the other.
        //
        // **Two passes**, because freeing an object touches other tables: what
        // dies is gathered first and freed after, rather than mutating ten
        // tables while iterating one.
        let mut bytes = 0usize;
        let mut live = 0usize;
        let mut slots_seen = 0usize;
        let mut dead: Vec<ObjectId> = Vec::new();
        #[cfg(feature = "profile")]
        let generation = self.collections as u32;
        #[cfg(feature = "profile")]
        let mut young_survived = 0u64;
        each_table!(&mut *self, table, kind, {
            let slot = table.slot_size();
            slots_seen += table.slots.len();
            for index in 0..table.slots.len() {
                if table.slots[index].is_none() {
                    continue;
                }
                if table.is_marked(index as u32) {
                    live += 1;
                    #[cfg(feature = "profile")]
                    if table.is_newborn(index as u32, generation) {
                        young_survived += 1;
                    }
                    if let Some(value) = table.slots[index].as_mut() {
                        shrink_settled(value);
                        bytes += slot + value.contents_size();
                    }
                } else {
                    dead.push(ObjectId::tagged(kind.tag(), index as u32));
                }
            }
        });

        for id in &dead {
            self.discard(*id);
        }
        self.compact_fields();
        // A full collection settles the generations too: whatever is still
        // here has survived one, so it is old, and no old object can be
        // pointing at anything young.
        if NURSERY {
            each_table!(&mut *self, table, _kind, {
                table.age_all();
                table.forget_all();
            });
            self.young_total = 0;
            self.young_bytes = 0;
        }

        #[cfg(feature = "profile")]
        {
            self.profile.collections += 1;
            self.profile.slots_swept += slots_seen as u64;
            self.profile.swept += before.saturating_sub(live) as u64;
            self.profile.live_before += before as u64;
            self.profile.survived += live as u64;
            self.profile.collect_nanos += started.elapsed().as_nanos() as u64;
            self.profile.young_survived += young_survived;
        }
        #[cfg(not(feature = "profile"))]
        let _ = slots_seen;

        self.live = live;
        self.bytes = bytes;
        self.threshold = match self.headroom {
            // No `INITIAL_THRESHOLD` here: a ceiling means what it says.
            Some(headroom) => bytes.saturating_add(headroom),
            None => {
                let (numerator, denominator) = self.growth;
                (bytes * numerator / denominator).max(INITIAL_THRESHOLD)
            }
        };
        self.collections += 1;

        Collection {
            before,
            after: live,
            bytes_after: bytes,
        }
    }

    /// **What a reference count would have managed on its own.**
    ///
    /// Everything unmarked is about to be freed. A refcounted heap would have
    /// freed some of it the instant the last reference went away, and leaked
    /// the rest -- a cycle keeps its own counts above zero for ever. Which is
    /// which is decided here by simulating the counts: build the in-degree
    /// *within the garbage* (nothing live can point at garbage, by definition
    /// of reachable), then repeatedly remove whatever has no incoming
    /// reference left. What cannot be removed is a cycle, or is reachable only
    /// from one.
    #[cfg(feature = "profile")]
    fn profile_garbage(&mut self) {
        use alloc::collections::BTreeMap;

        let garbage: Vec<ObjectId> = self
            .ids()
            .into_iter()
            .filter(|id| !self.is_marked_at(*id))
            .collect();
        let mut indegree: BTreeMap<u32, u32> = BTreeMap::new();
        for id in &garbage {
            indegree.entry(id.raw()).or_insert(0);
        }

        let mut referents: Vec<ObjectId> = Vec::new();
        for id in &garbage {
            referents.clear();
            self.trace_at(*id, &mut referents);
            for target in &referents {
                if let Some(count) = indegree.get_mut(&target.raw()) {
                    *count += 1;
                }
            }
        }

        let mut queue: Vec<u32> = indegree
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(raw, _)| *raw)
            .collect();
        let mut acyclic = 0u64;
        while let Some(raw) = queue.pop() {
            acyclic += 1;
            referents.clear();
            self.trace_at(ObjectId::new(raw), &mut referents);
            for target in &referents {
                if let Some(count) = indegree.get_mut(&target.raw()) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        queue.push(target.raw());
                    }
                }
            }
        }
        self.profile.garbage_acyclic += acyclic;
        self.profile.garbage_cyclic += garbage.len() as u64 - acyclic;
    }
    /// How many objects are alive.
    pub fn live(&self) -> usize {
        self.live
    }

    /// Roughly how many bytes those objects hold.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many collections have run.
    pub fn collections(&self) -> usize {
        self.collections
    }
}

/// Trace whatever is in a slot, if anything is.
fn trace_slot<T: crate::object::Trace>(slot: Option<&T>, gray: &mut Vec<ObjectId>) {
    if let Some(value) = slot {
        value.trace(gray);
    }
}

/// Hand back what a settled class's method table over-reserved.
///
/// A class's table is built by repeated `resize`, and a `Vec` that grows by
/// doubling ends up holding about half as much again as it uses. A class is
/// settled by the time it survives a collection -- Wren cannot add a method
/// after the body has run -- so this is the cheapest place to give it back.
///
/// It is a trait method rather than a `match` so that the sweep can stay
/// generic over the table's payload; every other type does nothing.
fn shrink_settled<T: crate::object::Trace>(value: &mut T) {
    value.settle();
}

impl Default for Heap {
    fn default() -> Heap {
        Heap::new()
    }
}
