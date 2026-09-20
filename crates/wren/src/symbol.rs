//! Interning for method signatures and variable names.
//!
//! A method call compiles to an index, not a string comparison: `Op::Call`
//! carries a symbol, and dispatch is an array index into the receiver class's
//! method table. This is where a name becomes that index, and it is upstream's
//! `SymbolTable` doing the same job.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A set of names, each with a stable index.
pub struct SymbolTable {
    names: Vec<String>,
}

impl SymbolTable {
    /// What this table has taken: the vector, and every name in it.
    ///
    /// **A signature is a `String`, so each one is its own allocation** --
    /// `"iteratorValue(_)"` is a 16-byte buffer plus the 12 bytes of the
    /// `String` that points at it. Interning 160 of them is the reason a VM
    /// that has run nothing still holds kilobytes.
    /// How many names this table has room for before it reallocates.
    #[cfg(feature = "census")]
    pub fn capacity(&self) -> usize {
        self.names.capacity()
    }

    #[cfg(feature = "census")]
    pub fn footprint(&self) -> usize {
        self.names.capacity() * core::mem::size_of::<String>()
            + self
                .names
                .iter()
                .map(|name| name.capacity())
                .sum::<usize>()
    }

    pub fn new() -> SymbolTable {
        SymbolTable { names: Vec::new() }
    }

    /// The index for `name`, adding it if it is new.
    ///
    /// **Linear.** Upstream's is too. It is called once per distinct name at
    /// compile time and never at run time, so the cost is bounded by the size
    /// of the program's vocabulary rather than by how often it runs — a hash
    /// map here would be more code and more memory for no measurable gain.
    pub fn ensure(&mut self, name: &str) -> usize {
        match self.find(name) {
            Some(index) => index,
            None => {
                self.names.push(name.to_string());
                self.names.len() - 1
            }
        }
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|existing| existing == name)
    }

    pub fn name(&self, index: usize) -> Option<&str> {
        self.names.get(index).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

impl Default for SymbolTable {
    fn default() -> SymbolTable {
        SymbolTable::new()
    }
}
