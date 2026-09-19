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
}

impl<T> Table<T> {
    fn new() -> Table<T> {
        Table {
            slots: Vec::new(),
            free: Vec::new(),
            marks: Vec::new(),
        }
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
    /// Set while a collection is not wanted — during a sequence of allocations
    /// whose intermediate results are not yet reachable from any root.
    paused: bool,
    collections: usize,
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
            paused: false,
            collections: 0,
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

    /// Push everything the object under `id` refers to onto the work list.
    fn trace_at(&self, id: ObjectId, gray: &mut Vec<ObjectId>) {
        let index = id.index();
        match ObjectType::from_tag(id.tag()) {
            Some(ObjectType::Class) => trace_slot(self.classes.get(index), gray),
            Some(ObjectType::Closure) => trace_slot(self.closures.get(index), gray),
            Some(ObjectType::Fn) => trace_slot(self.functions.get(index), gray),
            Some(ObjectType::Fiber) => trace_slot(self.fibers.get(index), gray),
            Some(ObjectType::Instance) => trace_slot(self.instances.get(index), gray),
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
        !self.paused && self.bytes >= self.threshold
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
        let mut bytes = 0usize;
        let mut live = 0usize;
        let mut slots_seen = 0usize;
        each_table!(&mut *self, table, _kind, {
            let slot = table.slot_size();
            slots_seen += table.slots.len();
            for index in 0..table.slots.len() {
                if table.slots[index].is_none() {
                    continue;
                }
                if table.is_marked(index as u32) {
                    live += 1;
                    if let Some(value) = table.slots[index].as_mut() {
                        shrink_settled(value);
                        bytes += slot + value.contents_size();
                    }
                } else {
                    // Dropping the payload releases whatever its `Vec`s held.
                    table.slots[index] = None;
                    table.free.push(index as u32);
                }
            }
        });

        #[cfg(feature = "profile")]
        {
            self.profile.collections += 1;
            self.profile.slots_swept += slots_seen as u64;
            self.profile.swept += before.saturating_sub(live) as u64;
            self.profile.live_before += before as u64;
            self.profile.survived += live as u64;
            self.profile.collect_nanos += started.elapsed().as_nanos() as u64;
        }
        #[cfg(not(feature = "profile"))]
        let _ = slots_seen;

        self.live = live;
        self.bytes = bytes;
        let (numerator, denominator) = self.growth;
        self.threshold = (bytes * numerator / denominator).max(INITIAL_THRESHOLD);
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
