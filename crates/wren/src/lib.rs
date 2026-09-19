//! Wren, re-implemented in Rust for microcontrollers.
//!
//! [Wren](https://wren.io/) is a small class-based scripting language with a
//! bytecode VM, closures, fibers and a garbage collector. This crate is a
//! re-implementation of it in Rust, built to run on parts with kilobytes rather
//! than megabytes.
//!
//! # What shapes the design
//!
//! **The smallest target has 8 KB of RAM.** That one number decides more than
//! any preference:
//!
//! * `no_std` by default. `std` is a feature, and it is for the host — tests,
//!   and eventually an ahead-of-time compiler that runs on a workstation.
//! * No dependencies. A firmware should be able to add this crate without
//!   pulling a tree of others behind it, and every byte of flash on a 62 KB
//!   part is spoken for.
//! * The lexer allocates nothing at all, and the compiler is separable from the
//!   VM — so a part too small to compile can still run bytecode built
//!   elsewhere.
//!
//! # The object representation
//!
//! This is the design's central feature, and most of the rest follows from it.
//!
//! A [`Value`] is **8 bytes**: an `f64` whose NaN payload carries everything
//! that is not a number, using upstream's constants. A tagged enum would be 16,
//! and doubling every stack slot, list element and instance field is not
//! affordable here.
//!
//! An object is reached by [`ObjectId`] — a 4-byte index into one table the
//! [`Heap`] owns — rather than by pointer, and **carries no header at all**.
//! Upstream spends 16 bytes per object on one: a type tag, a mark bit, a class
//! pointer and a `next` pointer threading it onto a global list. Here the type
//! tag is the enum discriminant folded into padding, the mark is a bit in a
//! bitmap beside the table, and the `next` pointer is unnecessary because the
//! table already enumerates every object.
//!
//! Three things follow:
//!
//! * **No `unsafe`.** A pointer-based object graph with a tracing collector
//!   needs it throughout; an index does not. A handle to a collected object is
//!   a failed lookup, never a read of freed memory — so this crate can and does
//!   `#![forbid(unsafe_code)]`.
//! * **The sweep scans words rather than chasing pointers**, which matters more
//!   than it sounds on a part with no cache worth the name.
//! * **The collector is replaceable.** Reclamation policy is confined to
//!   [`heap`]; nothing else learns how a lifetime is decided.
//!
//! What it costs, equally plainly: every object occupies 24 bytes before its
//! contents (the largest enum variant), variable-length data is a second
//! allocation where upstream inlines it, and every access is bounds-checked.
//! The crate README and `doc/wren-rs/design.md` carry the measured numbers and
//! the argument in full.
//!
//! # Status
//!
//! Early. The lexer, the value representation, the object model and a
//! mark-sweep collector over it are written and tested. The compiler and the
//! interpreter loop are not.
//!
//! See `doc/wren-rs/design.md` for why values are NaN-tagged and objects are
//! reached by handle rather than by pointer — the decision that makes the
//! collector replaceable later — and the repository README for how this is
//! being measured against upstream Wren and MicroPython on the same board.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
// Kilobytes, not megabytes: anything unused is flash somebody is paying for.
#![deny(dead_code)]

pub mod handle;
pub mod lexer;
pub mod math;

// The heap and the objects in it need a growable allocation. A part with no
// allocator at all still gets the lexer, and later the bytecode reader.
#[cfg(feature = "alloc")]
pub mod bytecode;
#[cfg(feature = "alloc")]
pub mod compiler;
#[cfg(feature = "alloc")]
pub mod core;
#[cfg(feature = "alloc")]
pub mod heap;
#[cfg(feature = "alloc")]
pub mod object;
#[cfg(feature = "alloc")]
pub mod symbol;
pub mod value;
#[cfg(feature = "alloc")]
pub mod vm;

pub use handle::ObjectId;
pub use lexer::{Lexer, Token, TokenKind};
pub use value::Value;

#[cfg(feature = "alloc")]
pub use heap::Heap;
#[cfg(feature = "alloc")]
pub use object::{ObjClass, ObjInstance, ObjList, ObjMap, ObjRange, ObjString, Object};
#[cfg(feature = "alloc")]
pub use vm::{Vm, WrenError};

/// The version of Wren this implementation targets.
///
/// Pinned to what `vendor/wren` holds, so that a behavioural difference is a
/// difference against a known reference rather than against "Wren" in general.
pub const TARGET_WREN_VERSION: &str = "0.4.0";
