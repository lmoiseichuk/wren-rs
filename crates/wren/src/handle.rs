//! Handles to heap objects.
//!
//! This is its own module, and not part of [`object`](crate::object), for one
//! reason: [`Value`](crate::value::Value) needs to be able to *hold* a handle
//! on a part with no allocator at all, where there is no heap and no object to
//! refer to. Keeping the handle here is what lets the lexer and the value
//! representation compile without `alloc`.

/// A handle to a heap object.
///
/// Four bytes, which is what a pointer would be on the 32-bit parts this
/// targets, so nothing is lost by the indirection in space — only the bounds
/// check on each access.
///
/// **A handle does not keep its object alive.** The collector decides that from
/// the roots, exactly as upstream's does. A handle to a swept object is stale,
/// and looking one up returns `None` rather than reading freed memory; that is
/// the property that lets this crate forbid `unsafe`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ObjectId(u32);

impl ObjectId {
    /// Build a handle from a raw index.
    ///
    /// Public because a bytecode loader has to rebuild handles it wrote, and
    /// because tests need to make one without a heap. **A handle built this way
    /// is not promised to refer to anything**; the heap answers `None` for one
    /// that does not, which is why this cannot be unsound even when it is
    /// wrong.
    pub fn new(index: u32) -> ObjectId {
        ObjectId(index)
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    /// How many low bits of a handle name the type rather than the object.
    ///
    /// Ten types need four. **They are the low bits, not the high ones**, for
    /// two reasons: a packed method table entry reserves the top bit to say
    /// "primitive", and a `Value` in a 32-bit-number build has only 21 bits of
    /// payload to hold a whole handle in. Low bits leave both of those alone
    /// and simply shorten the index.
    pub const TAG_BITS: u32 = 4;
    const TAG_MASK: u32 = (1 << Self::TAG_BITS) - 1;

    /// Build a handle naming both a type and a slot within that type's table.
    pub fn tagged(tag: u8, index: u32) -> ObjectId {
        ObjectId((index << Self::TAG_BITS) | (u32::from(tag) & Self::TAG_MASK))
    }

    /// Which type's table this handle indexes. See `ObjectType::from_tag`.
    pub fn tag(self) -> u8 {
        (self.0 & Self::TAG_MASK) as u8
    }

    /// Where in that table.
    pub fn index(self) -> u32 {
        self.0 >> Self::TAG_BITS
    }
}

const _: () = assert!(core::mem::size_of::<ObjectId>() == 4);
