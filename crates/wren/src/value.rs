//! The `Value` type: what a Wren variable holds.
//!
//! Wren is dynamically typed, so one storage location has to be able to hold a
//! number, a boolean, null, or a reference to any heap object. Upstream solves
//! this with **NaN tagging**, and so does this, with the same constants — see
//! `doc/wren-rs/design.md` for why, and for what it costs.
//!
//! # How NaN tagging works
//!
//! An IEEE-754 double has an 11-bit exponent. When every exponent bit is set
//! the value is either infinity (mantissa zero) or a NaN (mantissa non-zero).
//! That leaves **52 mantissa bits doing nothing** in every NaN, which is a lot
//! of room to hide something in.
//!
//! So: if a `Value`'s bits are not a quiet NaN, it *is* a double and means
//! itself. If they are, the low bits say what it really is. Numbers cost
//! nothing — they are stored as themselves — and everything else is a tag.
//!
//! ```text
//!  63                                                             0
//!  ┌─┬───────────┬─┬──────────────────────────────────────────────┐
//!  │S│ 11111111111│Q│                 payload                      │
//!  └─┴───────────┴─┴──────────────────────────────────────────────┘
//!   │      │       │
//!   │      │       └─ quiet bit: set, so this is a quiet NaN
//!   │      └───────── exponent all ones
//!   └──────────────── sign bit: set means "this payload is an object handle"
//! ```
//!
//! With the sign bit clear, the low three bits are a singleton tag — null,
//! true, false, or the internal `undefined`. With it set, the payload is an
//! [`ObjectId`].
//!
//! # Why this is safe Rust
//!
//! The usual NaN-tagging implementation reinterprets a pointer as a double,
//! which needs `unsafe`. Two things avoid it here: `f64::to_bits` and
//! `f64::from_bits` are safe conversions, and the payload is a 32-bit table
//! index rather than a pointer, so nothing is ever cast to or from one.

// **A conversion here is redundant in one build and load-bearing in the
// other.** `Bits` is `u64` or `u32` depending on the `f32` feature, so every
// widening in this file is a no-op for one of the two and required for the
// other. Silenced once, at the module, because each site is the same fact and
// five copies of the same `#[allow]` read as five separate decisions.
#![allow(clippy::unnecessary_cast, clippy::useless_conversion)]

use crate::handle::ObjectId;

/// What a Wren number is.
///
/// **`f64` is the language**, and the default. `f32` is a build for parts that
/// cannot afford doubles, and it is not Wren: see the module docs above and
/// `doc/wren-rs/design.md`.
#[cfg(not(feature = "f32"))]
pub type Num = f64;
#[cfg(feature = "f32")]
pub type Num = f32;

/// The unsigned integer of the same width as [`Num`], which is what a `Value`
/// actually stores.
#[cfg(not(feature = "f32"))]
type Bits = u64;
#[cfg(feature = "f32")]
type Bits = u32;

/// The sign bit. Set on a `Value` means the payload is an object handle.
///
/// Upstream uses the same bit for the same purpose, which is why a pointer
/// there must fit in 48 bits. Here the payload is a table index, so the
/// constraint is academic — but keeping the layout identical means the two
/// implementations can be read against each other.
#[cfg(not(feature = "f32"))]
const SIGN_BIT: Bits = 1 << 63;
#[cfg(feature = "f32")]
const SIGN_BIT: Bits = 1 << 31;

/// A quiet NaN with two extra bits set.
///
/// The top mantissa bit makes it *quiet* rather than signalling. The next bit
/// is set because some x86 instructions produce a NaN with that bit clear when
/// given certain inputs, and a real NaN arriving from arithmetic must never be
/// mistaken for a tagged value. Upstream calls this out in the same place; the
/// `f64` value is bit-identical to its `QNAN`.
///
/// The `f32` constant is the same three fields in the narrower layout:
/// exponent all ones (`0x7f80_0000`), the quiet bit (`0x0040_0000`) and the
/// bit below it (`0x0020_0000`).
#[cfg(not(feature = "f32"))]
const QNAN: Bits = 0x7ffc_0000_0000_0000;
#[cfg(feature = "f32")]
const QNAN: Bits = 0x7fe0_0000;

/// How much of a tagged value is left over to hold a handle.
///
/// **This is the one real constraint `f32` adds.** A double leaves 52 mantissa
/// bits and a handle is a 32-bit index, so nothing is lost. A single leaves 23,
/// of which `QNAN` spends two, so a handle gets 21 bits — 2,097,151 objects.
/// A heap that large would need 40 MB of slots, which is not a part this build
/// exists for, but [`Value::object`] asserts it in debug builds rather than
/// silently aliasing two objects onto one handle.
#[cfg(not(feature = "f32"))]
const PAYLOAD: Bits = 0xffff_ffff;
#[cfg(feature = "f32")]
const PAYLOAD: Bits = (1 << 21) - 1;

/// The low bits that distinguish the singletons, when the sign bit is clear.
const MASK_TAG: Bits = 7;

const TAG_NAN: Bits = 0;
const TAG_NULL: Bits = 1;
const TAG_FALSE: Bits = 2;
const TAG_TRUE: Bits = 3;
const TAG_UNDEFINED: Bits = 4;

/// A Wren value: a number, a boolean, null, or a handle to a heap object.
///
/// `Copy`, and deliberately so — it is one machine word plus a word on a 32-bit
/// part, and the interpreter loop moves these constantly. Note that this is
/// also precisely what would have to change to support reference counting,
/// since a `Copy` type cannot observe its own copies or its own death;
/// `doc/wren-rs/design.md` sets out what that would involve.
#[derive(Clone, Copy)]
pub struct Value(Bits);

impl Value {
    /// The `null` Wren programs can see.
    pub const NULL: Value = Value(QNAN | TAG_NULL);
    /// `true`.
    pub const TRUE: Value = Value(QNAN | TAG_TRUE);
    /// `false`.
    pub const FALSE: Value = Value(QNAN | TAG_FALSE);

    /// The internal `undefined`, which is **not** a value a program can hold.
    ///
    /// Upstream uses it for exactly two things and so does this: a module
    /// variable that has been forward-referenced but not yet declared, which
    /// only exists during compilation; and an unused slot in a map. If one of
    /// these reaches a program, that is a bug rather than a feature.
    pub const UNDEFINED: Value = Value(QNAN | TAG_UNDEFINED);

    /// A number.
    ///
    /// No tagging happens: a double that is not a NaN already means itself.
    /// A NaN that arrives here stays a NaN and is still a number — the extra
    /// bit in [`QNAN`] is what keeps it from colliding with a tag.
    pub fn num(value: Num) -> Value {
        Value(value.to_bits())
    }

    /// A boolean, as the corresponding singleton.
    pub fn bool(value: bool) -> Value {
        if value {
            Value::TRUE
        } else {
            Value::FALSE
        }
    }

    /// A handle to a heap object.
    pub fn object(id: ObjectId) -> Value {
        debug_assert!(
            u64::from(id.raw()) <= u64::from(PAYLOAD),
            "handle {} does not fit in this build's payload",
            id.raw()
        );
        Value(SIGN_BIT | QNAN | (id.raw() as Bits & PAYLOAD))
    }

    /// Is this a number?
    ///
    /// Anything that is not a quiet NaN is a double. This is the check the
    /// interpreter makes most often, which is why it is two instructions.
    pub fn is_num(self) -> bool {
        (self.0 & QNAN) != QNAN
    }

    /// Is this a handle to a heap object?
    pub fn is_object(self) -> bool {
        (self.0 & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT)
    }

    pub fn is_null(self) -> bool {
        self.0 == Value::NULL.0
    }

    pub fn is_true(self) -> bool {
        self.0 == Value::TRUE.0
    }

    pub fn is_false(self) -> bool {
        self.0 == Value::FALSE.0
    }

    pub fn is_bool(self) -> bool {
        self.is_true() || self.is_false()
    }

    pub fn is_undefined(self) -> bool {
        self.0 == Value::UNDEFINED.0
    }

    /// The number this holds, or `None` if it is not one.
    ///
    /// Upstream's `AS_NUM` does no checking and the caller is expected to have
    /// tested first. Returning an `Option` instead costs nothing once inlined
    /// and removes a whole class of mistake, so the unchecked form is not
    /// offered.
    pub fn as_num(self) -> Option<Num> {
        if self.is_num() {
            Some(Num::from_bits(self.0))
        } else {
            None
        }
    }

    /// The object handle this holds, or `None` if it is not one.
    pub fn as_object(self) -> Option<ObjectId> {
        if self.is_object() {
            // A truncation when `Bits` is `u64`, an identity when it is
            // `u32`; the mask above is what makes the truncation safe.
            Some(ObjectId::new((self.0 & PAYLOAD) as u32))
        } else {
            None
        }
    }

    /// Wren's notion of truthiness: **only `false` and `null` are falsy.**
    ///
    /// Zero is true, the empty string is true, an empty list is true. This
    /// catches people out coming from Python or JavaScript, and it is upstream's
    /// rule, not a simplification.
    pub fn is_falsy(self) -> bool {
        self.is_false() || self.is_null()
    }

    /// Bitwise identity — the fast path of Wren's `==`.
    ///
    /// **This is not full equality.** Two distinct string objects with the same
    /// contents are `==` in Wren but not identical here, and the comparison
    /// that knows about that needs the heap to look the strings up. The VM
    /// tries this first and falls back.
    ///
    /// One deliberate quirk, inherited from upstream: a NaN is identical to
    /// itself under this test, where IEEE-754 says `NaN != NaN`. Wren's `==`
    /// preserves the IEEE behaviour by testing numbers separately before
    /// reaching this.
    pub fn is_same(self, other: Value) -> bool {
        self.0 == other.0
    }

    /// The raw bits, for tests and for a bytecode writer that has to serialise
    /// a constant.
    pub fn to_bits(self) -> u64 {
        self.0 as u64
    }

    /// Rebuild a value from raw bits.
    ///
    /// Only for a bytecode reader loading constants it wrote itself. Feeding
    /// arbitrary bits in can produce a handle to an object that does not exist;
    /// the heap treats that as a lookup failure rather than as undefined
    /// behaviour, so it is safe in the Rust sense but still wrong.
    pub fn from_bits(bits: u64) -> Value {
        Value(bits as Bits)
    }
}

impl core::fmt::Debug for Value {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match () {
            _ if self.is_num() => write!(out, "Num({})", Num::from_bits(self.0)),
            _ if self.is_null() => write!(out, "Null"),
            _ if self.is_true() => write!(out, "True"),
            _ if self.is_false() => write!(out, "False"),
            _ if self.is_undefined() => write!(out, "Undefined"),
            _ if self.is_object() => write!(out, "Object({})", self.0 & PAYLOAD),
            // A quiet NaN that is not any known tag. Reachable only from
            // `from_bits` with something invented, so say so rather than lie.
            _ => write!(out, "Invalid(0x{:x})", self.0),
        }
    }
}

/// `TAG_NAN` exists in upstream to name the untagged-NaN case explicitly.
/// Nothing reads it, but leaving it out would make the tag list look like it
/// starts at one by accident rather than because zero is spoken for.
const _: () = assert!(TAG_NAN == 0);
const _: () = assert!(MASK_TAG == 7);

// The whole reason for NaN tagging: a tagged enum would be sixteen bytes, and
// every stack slot, list element and instance field would pay the difference.
#[cfg(not(feature = "f32"))]
const _: () = assert!(core::mem::size_of::<Value>() == 8);
#[cfg(feature = "f32")]
const _: () = assert!(core::mem::size_of::<Value>() == 4);

// The tags have to fit under the quiet bits, or a singleton would read back as
// a different one. Cheap to check and expensive to discover.
const _: () = assert!(TAG_UNDEFINED <= MASK_TAG);
const _: () = assert!(QNAN & MASK_TAG == 0);
const _: () = assert!(PAYLOAD & QNAN == 0);
