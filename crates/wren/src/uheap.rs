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
//! **This is not wired to the VM.** It is the allocator a `no_std` firmware
//! without one would hand to `alloc`, and it is here as a tested piece rather
//! than a promise. See the note at the bottom on what connecting it involves.

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
    /// How many records the table holds.
    ///
    /// **This is the third number and it carries the rest.** Blocks are cut
    /// downwards and records appended, so the last record is always the
    /// lowest block — which makes the boundary between the table and the
    /// blocks derivable rather than stored.
    count: u16,
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
        UHeap { memory, count: 0 }
    }

    /// How many units the heap spans in total.
    pub fn units(&self) -> usize {
        self.memory.len()
    }

    /// How many blocks exist, free ones included.
    pub fn blocks(&self) -> usize {
        self.count as usize
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
        if units == 0 || units > SIZE {
            return None;
        }

        // Best fit among the holes, so a large one is not spent on a small
        // ask when a snug hole exists.
        let mut best: Option<(u16, u16)> = None;
        for at in 0..self.count {
            let record = self.record(at);
            if record.free && record.size >= units {
                match best {
                    Some((_, size)) if size <= record.size => {}
                    _ => best = Some((at, record.size)),
                }
            }
        }
        if let Some((at, _)) = best {
            let mut record = self.record(at);
            record.free = false;
            self.write(at, record);
            return Some(record.index);
        }

        // Nothing reusable: cut a new block off the bottom. The table needs
        // one more unit too, and they grow towards each other.
        let floor = self.floor();
        if floor < units {
            return None;
        }
        let index = floor - units;
        if (index as usize) < self.count as usize + 1 {
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
        Some(index)
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

    /// How many units are spoken for, holes included.
    ///
    /// The figure a firmware wants is this one rather than the sum of live
    /// blocks: a hole that cannot be reused is as unavailable as a block.
    pub fn used(&self) -> usize {
        let floor = self.floor() as usize;
        (self.memory.len() - floor) + self.count as usize
    }
}
