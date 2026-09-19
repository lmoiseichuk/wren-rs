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

use crate::object::{Object, ObjectId};
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
    /// Set while a collection is not wanted — during a sequence of allocations
    /// whose intermediate results are not yet reachable from any root.
    paused: bool,
    collections: usize,
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
            paused: false,
            collections: 0,
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
        self.bytes += object.size_estimate();
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
            if let Some(object) = self.slots[index].as_ref() {
                object.trace(&mut gray);
            }
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
                bytes += self.slots[index].as_ref().map_or(0, Object::size_estimate);
            } else {
                // Dropping the `Object` releases whatever its `Vec`s held.
                self.slots[index] = None;
                self.free.push(index as u32);
            }
        }

        self.live = live;
        self.bytes = bytes;
        self.threshold = (bytes * GROWTH_NUMERATOR / GROWTH_DENOMINATOR).max(INITIAL_THRESHOLD);
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
