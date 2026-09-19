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
    ObjUpvalue, Object, ObjectId, ObjectType,
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

/// The object table.
pub struct Heap {
    /// `None` is a free slot. `Option<Object>` is the same size as `Object`
    /// here — the enum has spare discriminants for the niche — so empty slots
    /// cost nothing extra.
    slots: Vec<Option<Object>>,
    /// Indices of free slots, newest first. A separate stack rather than a
    /// linked list threaded through the slots themselves: the same asymptotics,
    /// none of the aliasing that a threaded list needs `unsafe` to express.
    free: Vec<u32>,
    /// One bit per slot. Cleared at the start of each mark phase.
    marks: Vec<u64>,
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
            slots: Vec::new(),
            free: Vec::new(),
            marks: Vec::new(),
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

    /// Put an object in the table and hand back a handle to it.
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
    pub fn allocate(&mut self, object: Object) -> ObjectId {
        #[cfg(feature = "profile")]
        {
            self.profile.allocated[object.object_type() as usize] += 1;
            self.profile.allocated_bytes += object.size_estimate() as u64;
        }
        self.bytes += object.size_estimate();
        #[cfg(feature = "profile")]
        {
            // Occupancy is exact and free: every slot that is not on the free
            // list holds something.
            let occupied = self.slots.len().saturating_sub(self.free.len()) + 1;
            self.profile.peak_live = self.profile.peak_live.max(occupied);
            self.profile.peak_bytes = self.profile.peak_bytes.max(self.bytes);
        }
        self.live += 1;

        match self.free.pop() {
            Some(index) => {
                self.slots[index as usize] = Some(object);
                ObjectId::new(index)
            }
            None => {
                let index = self.slots.len() as u32;
                self.slots.push(Some(object));
                // One more bit may need one more word.
                if self.marks.len() * 64 < self.slots.len() {
                    self.marks.push(0);
                }
                ObjectId::new(index)
            }
        }
    }

    /// Look an object up. `None` if the handle is stale or was never valid.
    pub fn get(&self, id: ObjectId) -> Option<&Object> {
        self.slots.get(id.raw() as usize)?.as_ref()
    }

    /// Look an object up for modification.
    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.slots.get_mut(id.raw() as usize)?.as_mut()
    }

    /// Look up an object of a known type.
    ///
    /// **This is the API the VM should use, not [`get`](Heap::get).** Almost
    /// every access already knows what it expects -- `match heap.get(id) {
    /// Some(Object::Class(class)) => ..., _ => return }` was the shape of 140
    /// sites -- and saying so lets the heap answer without building an enum
    /// the caller immediately takes apart.
    ///
    /// It is also what makes the storage replaceable. With one table per type,
    /// there is no single `Object` to hand back a reference to; there is a
    /// `&ObjClass` in the class table. These accessors are the boundary that
    /// lets that change without touching the VM.
    ///
    /// A handle of the wrong type reads as `None`, exactly as a stale handle
    /// does. That is the same contract `get` has and the same one upstream's
    /// `AS_CLASS` does not: there, asking a string for its methods is
    /// undefined behaviour.
    pub fn class(&self, id: ObjectId) -> Option<&ObjClass> {
        match self.get(id)? {
            Object::Class(class) => Some(class),
            _ => None,
        }
    }

    pub fn class_mut(&mut self, id: ObjectId) -> Option<&mut ObjClass> {
        match self.get_mut(id)? {
            Object::Class(class) => Some(class),
            _ => None,
        }
    }

    pub fn instance(&self, id: ObjectId) -> Option<&ObjInstance> {
        match self.get(id)? {
            Object::Instance(instance) => Some(instance),
            _ => None,
        }
    }

    pub fn instance_mut(&mut self, id: ObjectId) -> Option<&mut ObjInstance> {
        match self.get_mut(id)? {
            Object::Instance(instance) => Some(instance),
            _ => None,
        }
    }

    pub fn list(&self, id: ObjectId) -> Option<&ObjList> {
        match self.get(id)? {
            Object::List(list) => Some(list),
            _ => None,
        }
    }

    pub fn list_mut(&mut self, id: ObjectId) -> Option<&mut ObjList> {
        match self.get_mut(id)? {
            Object::List(list) => Some(list),
            _ => None,
        }
    }

    pub fn map(&self, id: ObjectId) -> Option<&ObjMap> {
        match self.get(id)? {
            Object::Map(map) => Some(map),
            _ => None,
        }
    }

    pub fn map_mut(&mut self, id: ObjectId) -> Option<&mut ObjMap> {
        match self.get_mut(id)? {
            Object::Map(map) => Some(map),
            _ => None,
        }
    }

    pub fn range(&self, id: ObjectId) -> Option<&ObjRange> {
        match self.get(id)? {
            Object::Range(range) => Some(range),
            _ => None,
        }
    }

    pub fn string(&self, id: ObjectId) -> Option<&ObjString> {
        match self.get(id)? {
            Object::String(string) => Some(string),
            _ => None,
        }
    }

    pub fn string_mut(&mut self, id: ObjectId) -> Option<&mut ObjString> {
        match self.get_mut(id)? {
            Object::String(string) => Some(string),
            _ => None,
        }
    }

    pub fn upvalue(&self, id: ObjectId) -> Option<&ObjUpvalue> {
        match self.get(id)? {
            Object::Upvalue(upvalue) => Some(upvalue),
            _ => None,
        }
    }

    pub fn upvalue_mut(&mut self, id: ObjectId) -> Option<&mut ObjUpvalue> {
        match self.get_mut(id)? {
            Object::Upvalue(upvalue) => Some(upvalue),
            _ => None,
        }
    }

    /// `Fn` is a keyword, so the accessor is spelled out.
    pub fn function(&self, id: ObjectId) -> Option<&ObjFn> {
        match self.get(id)? {
            Object::Fn(function) => Some(function),
            _ => None,
        }
    }

    pub fn function_mut(&mut self, id: ObjectId) -> Option<&mut ObjFn> {
        match self.get_mut(id)? {
            Object::Fn(function) => Some(function),
            _ => None,
        }
    }

    pub fn closure(&self, id: ObjectId) -> Option<&ObjClosure> {
        match self.get(id)? {
            Object::Closure(closure) => Some(closure),
            _ => None,
        }
    }

    pub fn closure_mut(&mut self, id: ObjectId) -> Option<&mut ObjClosure> {
        match self.get_mut(id)? {
            Object::Closure(closure) => Some(closure),
            _ => None,
        }
    }

    pub fn fiber(&self, id: ObjectId) -> Option<&ObjFiber> {
        match self.get(id)? {
            Object::Fiber(fiber) => Some(fiber),
            _ => None,
        }
    }

    pub fn fiber_mut(&mut self, id: ObjectId) -> Option<&mut ObjFiber> {
        match self.get_mut(id)? {
            Object::Fiber(fiber) => Some(fiber),
            _ => None,
        }
    }

    /// What kind of object a handle refers to, without reading the object.
    ///
    /// Today this loads the object and reads its discriminant. Once the type
    /// lives in the handle it will not touch the heap at all, which is what
    /// makes `class_of` free for every built-in -- one of the four heap
    /// lookups a method call still costs.
    pub fn type_of(&self, id: ObjectId) -> Option<ObjectType> {
        Some(self.get(id)?.object_type())
    }

    /// Every live handle, in allocation order.
    ///
    /// **Not a traversal the running VM does.** This exists for the one-off
    /// passes that have to see the whole heap -- flattening the class
    /// hierarchy after the core library is installed, and diagnostics. The
    /// collector does not use it: it walks from roots, which is the point of
    /// having roots.
    pub fn ids(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            if slot.is_none() {
                return None;
            }
            Some(ObjectId::new(index as u32))
        })
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

    /// What the collector has been doing. See [`Profile`].
    #[cfg(feature = "profile")]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The growth factor in force, as `(numerator, denominator)`.
    pub fn growth(&self) -> (usize, usize) {
        self.growth
    }

    /// Where the next collection is due, in estimated live bytes.
    pub fn threshold(&self) -> usize {
        self.threshold
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

        for word in &mut self.marks {
            *word = 0;
        }

        // Mark. A `Vec` as the work list rather than recursion: a deep object
        // graph would otherwise be a stack overflow, and on a part with 8 KB of
        // RAM the stack is the scarcest thing there is.
        let mut gray: Vec<ObjectId> = Vec::new();
        for root in roots {
            if let Some(id) = root.as_object() {
                gray.push(id);
            }
        }

        while let Some(id) = gray.pop() {
            let index = id.raw() as usize;
            if index >= self.slots.len() {
                continue;
            }
            if self.is_marked(index) {
                continue;
            }
            self.set_mark(index);
            #[cfg(feature = "profile")]
            {
                self.profile.marked += 1;
            }
            if let Some(object) = self.slots[index].as_ref() {
                object.trace(&mut gray);
            }
        }

        // **What a reference count would have managed on its own.**
        //
        // Everything unmarked is about to be freed. A refcounted heap would
        // have freed some of it the instant the last reference went away, and
        // leaked the rest -- a cycle keeps its own counts above zero for ever.
        // Which is which is decided here by simulating the counts: build the
        // in-degree *within the garbage* (nothing live can point at garbage,
        // by definition of reachable), then repeatedly remove whatever has no
        // incoming reference left. What cannot be removed is a cycle, or is
        // reachable only from one.
        //
        // This is the measurement that says whether refcounting can stand
        // alone or has to keep a tracing collector behind it.
        #[cfg(feature = "profile")]
        {
            let mut indegree: Vec<u32> = alloc::vec![0; self.slots.len()];
            let mut garbage: Vec<usize> = Vec::new();
            for index in 0..self.slots.len() {
                if self.slots[index].is_some() && !self.is_marked(index) {
                    garbage.push(index);
                }
            }

            let mut referents: Vec<ObjectId> = Vec::new();
            for index in &garbage {
                referents.clear();
                if let Some(object) = self.slots[*index].as_ref() {
                    object.trace(&mut referents);
                }
                for id in &referents {
                    let target = id.raw() as usize;
                    if target < self.slots.len()
                        && self.slots[target].is_some()
                        && !self.is_marked(target)
                    {
                        indegree[target] += 1;
                    }
                }
            }

            let mut queue: Vec<usize> = garbage
                .iter()
                .copied()
                .filter(|index| indegree[*index] == 0)
                .collect();
            let mut acyclic = 0u64;
            while let Some(index) = queue.pop() {
                acyclic += 1;
                referents.clear();
                if let Some(object) = self.slots[index].as_ref() {
                    object.trace(&mut referents);
                }
                for id in &referents {
                    let target = id.raw() as usize;
                    if target < self.slots.len()
                        && self.slots[target].is_some()
                        && !self.is_marked(target)
                    {
                        indegree[target] = indegree[target].saturating_sub(1);
                        if indegree[target] == 0 {
                            queue.push(target);
                        }
                    }
                }
            }
            self.profile.garbage_acyclic += acyclic;
            self.profile.garbage_cyclic += garbage.len() as u64 - acyclic;
        }

        // Sweep.
        let mut bytes = 0;
        let mut live = 0;
        for index in 0..self.slots.len() {
            if self.slots[index].is_none() {
                continue;
            }
            if self.is_marked(index) {
                live += 1;
                // **Give back what a growing table over-reserved.** A class's
                // method table is built by repeated `resize`, and a `Vec` that
                // grows geometrically ends up holding about half as much again
                // as it uses. Measured across the classes `binary_trees` has
                // live, that was 14,560 B held for nothing -- 14% of the live
                // heap, on a part with 320 KB.
                //
                // Only classes, and only here. A class is settled by the time
                // it survives a collection: Wren has no way to add a method
                // after the body has run, so the table will not grow again. A
                // list or a string is a different matter -- shrinking one that
                // is still being appended to would buy a realloc on the next
                // push and give the memory straight back.
                if let Some(Object::Class(class)) = self.slots[index].as_mut() {
                    class.methods.shrink_to_fit();
                }
                bytes += self.slots[index].as_ref().map_or(0, Object::size_estimate);
            } else {
                // Dropping the `Object` releases whatever its `Vec`s held.
                self.slots[index] = None;
                self.free.push(index as u32);
            }
        }

        #[cfg(feature = "profile")]
        {
            self.profile.collections += 1;
            self.profile.slots_swept += self.slots.len() as u64;
            self.profile.swept += before.saturating_sub(live) as u64;
            self.profile.live_before += before as u64;
            self.profile.survived += live as u64;
            self.profile.collect_nanos += started.elapsed().as_nanos() as u64;
        }

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

    fn is_marked(&self, index: usize) -> bool {
        self.marks[index / 64] & (1 << (index % 64)) != 0
    }

    fn set_mark(&mut self, index: usize) {
        self.marks[index / 64] |= 1 << (index % 64);
    }

    /// How many objects are alive.
    pub fn live(&self) -> usize {
        self.live
    }

    /// Estimated bytes held by live objects. See
    /// [`Object::size_estimate`](crate::object::Object::size_estimate) for what
    /// "estimated" is doing there.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many collections have run. The number a benchmark should report
    /// beside a time, since a run that never collected is measuring something
    /// else.
    pub fn collections(&self) -> usize {
        self.collections
    }
}

impl Default for Heap {
    fn default() -> Heap {
        Heap::new()
    }
}
