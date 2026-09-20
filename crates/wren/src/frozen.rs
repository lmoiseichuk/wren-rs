//! A core library built into the image rather than into RAM.
//!
//! **What this is for.** A tailored core is settled when the image is built:
//! the manifest fixes which classes exist, which methods they answer to, and
//! -- because `core::install` runs the same `define` calls in the same order
//! -- the number every method symbol gets. None of that has to be discovered
//! at start-up, and on a part with kilobytes of RAM none of it should be:
//! `fib`'s twenty-one classes are 1,092 bytes of `ObjClass` and 380 bytes of
//! method table that are identical on every boot.
//!
//! A [`FrozenCore`] is that state, emitted as Rust by
//! `cargo run --example freeze -- <program>.wrenc` and compiled into the
//! firmware. Together with the `.wrenc` itself, which a firmware already
//! includes with `include_bytes!`, it makes the whole program -- bytecode,
//! classes, method tables and names -- part of the image, leaving RAM for what
//! the program actually computes.
//!
//! # What still runs at start-up, and why that is the point
//!
//! `core::install` still runs, and it has to: it interns the method
//! signatures, it pushes the primitive function pointers that the frozen
//! method entries index into, and it defines the module-level variables. What
//! it no longer does is build any classes -- every `define` into a frozen
//! class is a no-op, because [`Heap::class_mut`](crate::heap::Heap::class_mut)
//! refuses one.
//!
//! That is not a special mode. `Vm::bind_primitive` pushes the primitive
//! *before* it looks the class up, so the primitive numbering is identical
//! whether or not the class is frozen -- which is exactly what makes a
//! generated method table valid against a table of primitives the firmware
//! builds at run time.
//!
//! # Generate it with the features it will be compiled with
//!
//! **This is the way to get it wrong.** Which `define` calls `install` makes
//! is decided by the cargo features -- a build without `core_full` installs
//! fewer of them -- and the primitive index a method entry holds is just the
//! position of its `define` in that sequence. Generate against one feature set
//! and compile against another and every entry points at a plausible, wrong
//! primitive.
//!
//! The manifest filters which *signatures* are interned either way, so the
//! symbol table does not move when this happens and comparing symbols alone
//! reports agreement. [`FrozenCore::primitives`] is what actually catches it,
//! and [`FrozenCore::disagreement`] checks that first.

extern crate alloc;

use crate::object::ObjClass;

/// A core library computed when the image was built.
///
/// Generated; see the module documentation. The three tables are positional
/// and must be in the order the generator emitted them -- a class's name is
/// `class_names[i]` for `classes[i]`, and both become handles at index `i`.
pub struct FrozenCore {
    /// Each class's name, in class-handle order. Allocated as heap strings at
    /// start-up so that string handle `i` names class handle `i`, which is the
    /// invariant `ObjClass::frozen`'s `name` argument was generated against.
    ///
    /// **Still in RAM, unlike the classes.** An `ObjString` owns its bytes,
    /// and giving it a borrowed form is a separate change; these are 209 bytes
    /// on `fib` against the 1,472 the classes were.
    pub class_names: &'static [&'static str],
    /// The classes themselves, in handle order, with their method tables
    /// already flattened.
    pub classes: &'static [ObjClass],
    /// Every method signature, in symbol order.
    ///
    /// **Checked, not trusted.** The firmware interns these itself by running
    /// `install`; this copy is what
    /// [`FrozenCore::disagreement`] compares that against, so a generated
    /// module that has drifted from the crate says so instead of dispatching
    /// to the wrong method.
    pub symbols: &'static [&'static str],
    /// How many primitives `install` had pushed when this was generated.
    ///
    /// **The check that catches a feature mismatch.** A frozen method entry
    /// holds a primitive *index*, and those are handed out by the order
    /// `install` makes its `define` calls -- which the cargo features decide:
    /// a build without `core_full` makes fewer of them, so the same signature
    /// ends up at a different index. The symbol list does not move when that
    /// happens, because the manifest filters which signatures are interned
    /// either way, so comparing symbols alone reports agreement and the VM
    /// then calls the wrong primitive.
    ///
    /// This does move, and it is one `usize`.
    pub primitives: usize,
}

impl FrozenCore {
    /// The handle of the class with this name, if the core has one.
    ///
    /// **By name rather than by position.** An earlier version handed classes
    /// out in order, on the argument that the firmware makes the same calls in
    /// the same sequence the generator did -- which was true in `Vm::build`
    /// and false the moment `install_system` built a class of its own without
    /// going through the same helper. A cursor that is one ahead is not an
    /// error, it is every later class silently being some other class.
    ///
    /// Linear, over twenty-odd names, once at start-up.
    pub fn class_named(&self, name: &str) -> Option<crate::handle::ObjectId> {
        let index = self
            .class_names
            .iter()
            .position(|candidate| *candidate == name)?;
        Some(crate::handle::ObjectId::tagged(
            crate::object::ObjectType::Class.tag(),
            index as u32,
        ))
    }

    /// Why this core does not match the VM built from it, if it does not.
    ///
    /// **A frozen method table is a list of symbol numbers**, so a core
    /// generated against a different version of `core::install` -- one method
    /// added, one `define` moved -- would index the right table with the wrong
    /// numbers and call the wrong primitive. Nothing about that would look
    /// like an error at run time.
    ///
    /// Comparing the interned symbols catches it, because any change to the
    /// set of `define` calls or their order changes this list. Callers that
    /// can afford it should check at start-up; the host test does.
    pub fn disagreement(
        &self,
        interned: &crate::symbol::SymbolTable,
        primitives: usize,
    ) -> Option<alloc::string::String> {
        use alloc::string::ToString;
        if primitives != self.primitives {
            return Some(alloc::format!(
                "the frozen core was generated against {} primitives, this build \
                 registers {primitives} -- it was almost certainly generated with a \
                 different feature set (`core_full`, `nofp`), and every method entry \
                 in it indexes the wrong primitive",
                self.primitives
            ));
        }
        if interned.len() != self.symbols.len() {
            return Some(alloc::format!(
                "the frozen core has {} symbols, this build interns {}",
                self.symbols.len(),
                interned.len()
            ));
        }
        for (symbol, expected) in self.symbols.iter().enumerate() {
            match interned.name(symbol) {
                Some(actual) if actual == *expected => {}
                Some(actual) => {
                    return Some(alloc::format!(
                        "symbol {symbol} is '{actual}' here and '{expected}' in the frozen core"
                    ))
                }
                None => return Some("the interned table is shorter than it says".to_string()),
            }
        }
        None
    }
}
