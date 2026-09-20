//! A fixed-size heap for a part that has no allocator.
//!
//! **Three numbers and a buffer.** A starting pointer, a size, and a count of
//! how many blocks exist. Everything else is derived, which is what keeps it
//! usable on a chip with eight kilobytes of RAM.
//!
//! ```text
//!   unit 0                                                        end
//!   +--------------------+..............................+-----------+
//!   | record 0 .. N-1    |        unallocated           | block N-1 |
//!   +--------------------+..............................+-----------+
//!   the table grows up ->                           <- blocks grow down
//! ```
//!
//! The table of block records grows from the front; blocks are cut from the
//! back. They meet in the middle, and the heap is full when they would
//! overlap — so a byte is never spent on a free list threaded through the
//! blocks themselves, and no part of the buffer is ever cleared. A fresh heap
//! costs one store.
//!
//! **Everything is counted in units, not bytes.** A unit is
//! [`core::mem::size_of::<usize>()`] on the target, so a `u16` index reaches
//! 65,535 units — 256 KB on a 32-bit part, which is far past anything this is
//! for. Alignment is free for the same reason: every block starts on a unit.
//!
//! [`Arena`] wraps it as a `#[global_allocator]`, which is how a firmware
//! without one gets `alloc` — and therefore the VM.

/// A block's size, with the high bit meaning the block is free.
///
/// Fifteen bits of size leaves 32,767 units — 128 KB — for a single block,
/// against 256 KB for the whole heap. A part that wants one allocation half
/// the size of its RAM is not the part this is for.
const FREE: u16 = 1 << 15;
const SIZE: u16 = FREE - 1;

/// One unit of the buffer, which is also one record.
pub type Unit = usize;

/// A fixed-size heap over a buffer somebody else owns.
pub struct UHeap<'a> {
    /// The buffer, in units. Records live at the front, blocks at the back.
    memory: &'a mut [Unit],
    /// The most units ever spoken for, table included.
    ///
    /// **Peak is the number a fixed buffer lives by.** What is in use when a
    /// run finishes says nothing about whether it fitted.
    peak: u16,
    /// How many allocations were refused.
    ///
    /// A firmware that refuses one is a firmware that is about to panic, so
    /// the count is worth having even though it is usually zero -- it turns
    /// "it crashed" into "it wanted more than this buffer at its worst".
    refused: u16,
    /// How many records the table holds.
    ///
    /// **This is the third number and it carries the rest.** Blocks are cut
    /// downwards and records appended, so the last record is always the
    /// lowest block — which makes the boundary between the table and the
    /// blocks derivable rather than stored.
    count: u16,
    /// Bytes callers have asked for, against the bytes actually cut.
    ///
    /// **The difference is the allocator's own overhead**, and on a fixed
    /// heap it is worth a number rather than an estimate: a block rounds up
    /// to a whole unit, an aligned block absorbs the gap its rounding leaves,
    /// and a reused hole is taken whole however small the ask. Behind the
    /// census feature, so a shipping build carries neither the field nor the
    /// addition.
    #[cfg(feature = "census")]
    requested: usize,
    /// Units lost to a reused hole being bigger than the ask, and to a fresh
    /// cut absorbing its own alignment gap -- the two halves of the overhead,
    /// counted apart because they have different fixes.
    #[cfg(feature = "census")]
    wasted_reuse: usize,
    #[cfg(feature = "census")]
    wasted_cut: usize,
}

/// Where a block sits and how big it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Record {
    /// First unit of the block.
    index: u16,
    /// Units the block spans, without the free bit.
    size: u16,
    free: bool,
}

impl<'a> UHeap<'a> {
    /// A heap over `memory`, which is not cleared.
    ///
    /// **No `memset`.** Nothing reads a unit before it is written: a record is
    /// written when it is appended, and a block's contents belong to whoever
    /// asked for it. Clearing 8 KB at boot would be the single most expensive
    /// thing this module did.
    pub fn new(memory: &'a mut [Unit]) -> UHeap<'a> {
        UHeap {
            memory,
            count: 0,
            peak: 0,
            refused: 0,
            #[cfg(feature = "census")]
            requested: 0,
            #[cfg(feature = "census")]
            wasted_reuse: 0,
            #[cfg(feature = "census")]
            wasted_cut: 0,
        }
    }

    /// How many units the heap spans in total.
    pub fn units(&self) -> usize {
        self.memory.len()
    }

    /// How many blocks exist, free ones included.
    pub fn blocks(&self) -> usize {
        self.count as usize
    }

    /// Every block, in the order the table holds them: size in bytes, and
    /// whether it is a hole.
    ///
    /// **The other side of the accounting.** `Heap::memory_census` says what
    /// the VM asked for; this says what the allocator actually cut, alignment
    /// rounding and holes included. The two disagreeing by more than a few
    /// per cent means something is being paid for that nobody asked for --
    /// which is exactly the question a fixed heap exists to answer.
    ///
    /// Calls `visit` per block rather than returning a vector, because
    /// allocating one here would change the thing being measured.
    pub fn walk(&self, mut visit: impl FnMut(usize, bool)) {
        for at in 0..self.count {
            let record = self.record(at);
            visit(record.size as usize * ::core::mem::size_of::<Unit>(), record.free);
        }
    }

    /// The lowest unit any block occupies, which is where the table must stop.
    ///
    /// Derived rather than stored: blocks are cut downwards and records are
    /// appended, so the last record is the lowest block.
    fn floor(&self) -> u16 {
        match self.count {
            0 => self.memory.len() as u16,
            count => self.record(count - 1).index,
        }
    }

    fn record(&self, at: u16) -> Record {
        let packed = self.memory[at as usize];
        let index = (packed >> 16) as u16;
        let size = (packed & 0xffff) as u16;
        Record {
            index,
            size: size & SIZE,
            free: size & FREE != 0,
        }
    }

    fn write(&mut self, at: u16, record: Record) {
        let size = record.size | if record.free { FREE } else { 0 };
        self.memory[at as usize] = ((record.index as Unit) << 16) | size as Unit;
    }

    /// Take `units` of the heap, or `None` if it will not fit.
    ///
    /// Returns the block's first unit, which is its handle: the record's
    /// *position* cannot be, because merging moves records and a handle that
    /// moved under its owner would be worse than no handle at all.
    ///
    /// **A reused hole is taken whole.** Splitting one would put the
    /// allocated half at a higher address than the free remainder, which
    /// breaks the descending order the table depends on — and buying that
    /// back would cost more than the waste on a heap this size.
    pub fn alloc(&mut self, units: u16) -> Option<u16> {
        self.alloc_aligned(units, 1)
    }

    /// Take `units`, starting on a multiple of `align` units.
    ///
    /// **Alignment is why this exists.** A unit is a `usize`, so a block is
    /// naturally aligned for anything `usize`-sized -- and `alloc` is asked
    /// for eight-byte alignment on a thirty-two-bit part whenever something
    /// holds a `u64`. Refusing those is refusing to be an allocator; rounding
    /// the block's start down to the alignment costs at most one unit and
    /// keeps the block index derivable from the pointer, which is what makes
    /// `free` a subtraction rather than a header.
    pub fn alloc_aligned(&mut self, units: u16, align: u16) -> Option<u16> {
        let align = align.max(1);
        if units == 0 || units > SIZE || !align.is_power_of_two() {
            self.refused = self.refused.saturating_add(1);
            return None;
        }

        // Best fit among the holes, so a large one is not spent on a small
        // ask when a snug hole exists.
        let mut best: Option<(u16, u16)> = None;
        for at in 0..self.count {
            let record = self.record(at);
            if record.free && record.size >= units && record.index.is_multiple_of(align) {
                match best {
                    Some((_, size)) if size <= record.size => {}
                    _ => best = Some((at, record.size)),
                }
            }
        }
        if let Some((at, _)) = best {
            let mut record = self.record(at);
            record.free = false;
            #[cfg(feature = "census")]
            {
                self.wasted_reuse += (record.size - units) as usize
                    * ::core::mem::size_of::<Unit>();
            }
            self.write(at, record);
            return Some(record.index);
        }


        // Nothing reusable: cut a new block off the bottom. The table needs
        // one more unit too, and they grow towards each other.
        let floor = self.floor();
        if floor < units {
            self.refused = self.refused.saturating_add(1);
            return None;
        }
        // Rounded down, so the block starts on the alignment. The gap that
        // leaves belongs to this block rather than becoming an untracked
        // hole -- a hole nothing records is a hole nothing can reuse.
        let index = (floor - units) / align * align;
        #[cfg(feature = "census")]
        {
            self.wasted_cut += (floor - index - units) as usize * ::core::mem::size_of::<Unit>();
        }
        let units = floor - index;
        if (index as usize) < self.count as usize + 1 {
            self.refused = self.refused.saturating_add(1);
            return None;
        }
        let at = self.count;
        self.count += 1;
        self.write(
            at,
            Record {
                index,
                size: units,
                free: false,
            },
        );
        self.peak = self.peak.max(self.used() as u16);
        Some(index)
    }

    /// Note a request in bytes, which only the `GlobalAlloc` shim still knows.
    ///
    /// `alloc_aligned` is given units, so by the time a block is cut the
    /// caller's real ask has already been rounded up and the overhead it
    /// implies is no longer visible.
    #[cfg(feature = "census")]
    fn note_request(&mut self, bytes: usize) {
        self.requested += bytes;
    }

    #[cfg(feature = "census")]
    fn note_release(&mut self, bytes: usize) {
        self.requested = self.requested.saturating_sub(bytes);
    }

    /// Bytes callers currently hold, as they asked for them.
    ///
    /// Against [`UHeap::used`] this is the allocator's overhead: rounding to
    /// a whole unit, the gap an aligned block absorbs, and a reused hole
    /// taken whole.
    #[cfg(feature = "census")]
    pub fn requested(&self) -> usize {
        self.requested
    }

    /// The overhead, split by cause: `(reused holes, alignment gaps)`.
    ///
    /// Cumulative over the heap's life rather than current, because both are
    /// decisions made at the moment a block is cut and a later free does not
    /// take them back.
    #[cfg(feature = "census")]
    pub fn wasted(&self) -> (usize, usize) {
        (self.wasted_reuse, self.wasted_cut)
    }

    /// Give a block back.
    ///
    /// Three things happen, in this order, and the order is the design:
    /// the block is marked free; it is merged with either neighbour that is
    /// also free, so two holes never sit side by side; and then every free
    /// record on the end of the table is dropped, which hands that space back
    /// to the boundary rather than leaving it as a hole.
    pub fn free(&mut self, index: u16) -> bool {
        let Some(at) = self.find(index) else {
            return false;
        };
        let mut record = self.record(at);
        if record.free {
            // Freeing twice is a caller's fault and saying so is cheaper than
            // corrupting the table over it.
            return false;
        }
        record.free = true;
        self.write(at, record);

        // Records descend in address, so `at + 1` is the block *below* this
        // one and `at - 1` is the block above.
        if at + 1 < self.count && self.record(at + 1).free {
            self.merge(at, at + 1);
        }
        if at > 0 && self.record(at - 1).free {
            self.merge(at - 1, at);
        }

        // A free block on the end is not a hole, it is unallocated space.
        while self.count > 0 && self.record(self.count - 1).free {
            self.count -= 1;
        }
        true
    }

    /// Fold the block at `lower` into the record at `upper`, and close the gap.
    ///
    /// `upper` is the higher address and `lower` the one beneath it, so the
    /// merged block starts where `lower` starts and spans both.
    fn merge(&mut self, upper: u16, lower: u16) {
        let above = self.record(upper);
        let below = self.record(lower);
        self.write(
            upper,
            Record {
                index: below.index,
                size: above.size + below.size,
                free: true,
            },
        );
        for at in lower..self.count - 1 {
            let next = self.record(at + 1);
            self.write(at, next);
        }
        self.count -= 1;
    }

    fn find(&self, index: u16) -> Option<u16> {
        (0..self.count).find(|at| self.record(*at).index == index)
    }

    /// The block at `index`, as units.
    pub fn block(&mut self, index: u16, units: u16) -> &mut [Unit] {
        let start = index as usize;
        &mut self.memory[start..start + units as usize]
    }

    /// The most units ever spoken for.
    pub fn peak(&self) -> usize {
        self.peak as usize
    }

    /// How many allocations this heap has refused.
    pub fn refused(&self) -> usize {
        self.refused as usize
    }

    /// The largest run of free units an allocation could still take.
    ///
    /// Two numbers make a heap, not one: what is left, and the largest piece
    /// of it. A heap with plenty free and no piece big enough is full in the
    /// only sense that matters.
    pub fn largest_free(&self) -> usize {
        let mut largest = 0;
        for at in 0..self.count {
            let record = self.record(at);
            if record.free {
                largest = largest.max(record.size as usize);
            }
        }
        // The unallocated middle, less the unit a new record would need.
        let middle = (self.floor() as usize).saturating_sub(self.count as usize + 1);
        largest.max(middle)
    }

    /// How many units are spoken for, holes included.
    ///
    /// The figure a firmware wants is this one rather than the sum of live
    /// blocks: a hole that cannot be reused is as unavailable as a block.
    pub fn used(&self) -> usize {
        let floor = self.floor() as usize;
        (self.memory.len() - floor) + self.count as usize
    }
}


/// A [`UHeap`] over a buffer of its own, usable as a `#[global_allocator]`.
///
/// ```ignore
/// #[global_allocator]
/// static ALLOCATOR: wren::uheap::Arena<2048> = wren::uheap::Arena::new();
/// ```
///
/// `UNITS` is the size in units, so `2048` is 8 KB on a 32-bit part — a
/// CH32V006's entire RAM.
///
/// **Single-threaded by construction, and that is the whole safety
/// argument.** A global allocator has to be reachable without one, so this
/// holds its state in `UnsafeCell` and claims `Sync`. That claim is only
/// true on a target with one core where nothing that allocates can preempt
/// something else that allocates — which is the firmware this exists for,
/// and is not something the crate can check. A build with threads or with
/// allocating interrupt handlers must not use it.
pub struct Arena<const UNITS: usize> {
    memory: core::cell::UnsafeCell<[Unit; UNITS]>,
    heap: core::cell::UnsafeCell<Option<UHeap<'static>>>,
}

// SAFETY: as documented on the type -- one thread, no allocating preemption.
#[allow(unsafe_code)]
unsafe impl<const UNITS: usize> Sync for Arena<UNITS> {}

impl<const UNITS: usize> Default for Arena<UNITS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const UNITS: usize> Arena<UNITS> {
    /// An empty arena. The buffer is not cleared and is never read unwritten.
    pub const fn new() -> Arena<UNITS> {
        Arena {
            memory: core::cell::UnsafeCell::new([0; UNITS]),
            heap: core::cell::UnsafeCell::new(None),
        }
    }

    /// How many bytes are spoken for, holes included.
    pub fn used(&self) -> usize {
        // SAFETY: single-threaded, as documented on the type.
        #[allow(unsafe_code)]
        unsafe {
            self.heap_mut().used() * core::mem::size_of::<Unit>()
        }
    }

    /// How many blocks exist, free ones included.
    pub fn blocks(&self) -> usize {
        // SAFETY: as above.
        #[allow(unsafe_code)]
        unsafe {
            self.heap_mut().blocks()
        }
    }

    /// Everything a firmware wants to know, in one go.
    ///
    /// `(used, peak, largest free run, blocks, refusals)`, all in bytes
    /// except the last two. **Peak and the largest free run are the two that
    /// decide whether a buffer is big enough**: what is in use at the end
    /// says nothing, and free space that is not in one piece cannot be
    /// handed out.
    pub fn stats(&self) -> (usize, usize, usize, usize, usize) {
        let unit = core::mem::size_of::<Unit>();
        // SAFETY: as above.
        #[allow(unsafe_code)]
        let heap = unsafe { self.heap_mut() };
        (
            heap.used() * unit,
            heap.peak() * unit,
            heap.largest_free() * unit,
            heap.blocks(),
            heap.refused(),
        )
    }

    /// Every block the arena has cut: size in bytes, and whether it is a hole.
    ///
    /// See [`UHeap::walk`]. A firmware uses this to bucket its own blocks --
    /// a histogram of what a VM actually asks for says more about where to
    /// look than a single total does.
    pub fn walk(&self, visit: impl FnMut(usize, bool)) {
        // SAFETY: as above.
        #[allow(unsafe_code)]
        let heap = unsafe { self.heap_mut() };
        heap.walk(visit);
    }

    /// Bytes callers hold as they asked for them; see [`UHeap::requested`].
    #[cfg(feature = "census")]
    pub fn requested(&self) -> usize {
        // SAFETY: as above.
        #[allow(unsafe_code)]
        let heap = unsafe { self.heap_mut() };
        heap.requested()
    }

    /// The overhead, split by cause; see [`UHeap::wasted`].
    #[cfg(feature = "census")]
    pub fn wasted(&self) -> (usize, usize) {
        // SAFETY: as above.
        #[allow(unsafe_code)]
        let heap = unsafe { self.heap_mut() };
        heap.wasted()
    }

    /// The whole arena, in bytes.
    pub const fn capacity(&self) -> usize {
        UNITS * core::mem::size_of::<Unit>()
    }

    /// The heap, built on first use.
    ///
    /// Lazily, because the runtime may allocate before `main` runs and there
    /// is nowhere earlier to build it.
    ///
    /// # Safety
    ///
    /// The caller must not be reentering this from another thread; see the
    /// type's documentation.
    #[allow(unsafe_code, clippy::mut_from_ref)]
    unsafe fn heap_mut(&self) -> &mut UHeap<'static> {
        let slot = &mut *self.heap.get();
        if slot.is_none() {
            let memory: &'static mut [Unit] = &mut *self.memory.get();
            *slot = Some(UHeap::new(memory));
        }
        slot.as_mut().expect("just built")
    }

    fn base(&self) -> *mut Unit {
        self.memory.get().cast()
    }
}

#[allow(unsafe_code)]
unsafe impl<const UNITS: usize> core::alloc::GlobalAlloc for Arena<UNITS> {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        // Every block starts on a unit, so alignment up to one is free and
        // anything larger is refused rather than quietly misaligned.
        let unit = core::mem::size_of::<Unit>();
        let units = layout.size().div_ceil(unit);
        let align = layout.align().div_ceil(unit).max(1);
        let (Ok(units), Ok(align)) = (u16::try_from(units), u16::try_from(align)) else {
            return core::ptr::null_mut();
        };
        // SAFETY: single-threaded, as documented on the type.
        match self.heap_mut().alloc_aligned(units, align) {
            Some(index) => {
                #[cfg(feature = "census")]
                self.heap_mut().note_request(layout.size());
                self.base().add(index as usize).cast()
            }
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, _layout: core::alloc::Layout) {
        // The handle is the block's unit index and the pointer *is* the
        // block, so the index is a subtraction rather than a header.
        let offset = pointer.cast::<Unit>().offset_from(self.base());
        // SAFETY: as above.
        #[cfg(feature = "census")]
        self.heap_mut().note_release(_layout.size());
        self.heap_mut().free(offset as u16);
    }
}
