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
//! # Status
//!
//! Early. The lexer works and is tested; the compiler, VM and collector are not
//! written. See the repository README for the plan and for how this is being
//! measured against upstream Wren and MicroPython on the same board.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
// Kilobytes, not megabytes: anything unused is flash somebody is paying for.
#![deny(dead_code)]

pub mod lexer;

pub use lexer::{Lexer, Token, TokenKind};

/// The version of Wren this implementation targets.
///
/// Pinned to what `vendor/wren` holds, so that a behavioural difference is a
/// difference against a known reference rather than against "Wren" in general.
pub const TARGET_WREN_VERSION: &str = "0.4.0";
