//! The program store, and looking a program up in it.
//!
//! **"Looking it up" is the point, not a formality.** A node does not run a
//! program because the firmware was built around it; it runs whatever it
//! finds under a known name, and carries on when a name is absent. That is
//! what makes `boot` optional and what makes either file replaceable without
//! touching the other. The store here is a table in flash rather than a
//! filesystem, but the shape of the question -- "is there a `boot`?" -- is the
//! same one a node with a filesystem asks, and so is the answer when there
//! isn't.
//!
//! Which extension is looked for is decided by how the firmware was built. A
//! build with no compiler cannot do anything with a `.wren` and does not look
//! for one.

/// One program as the device holds it: a name, and the bytes under it.
pub struct Program {
    /// The name as it would appear on a filesystem, extension included.
    pub name: &'static str,
    /// Source for a compiler build, bytecode otherwise.
    pub bytes: &'static [u8],
}

/// The extension this image can actually execute.
///
/// `cfg!` rather than two `#[cfg]` constants: it expands to a plain `true` or
/// `false` at compile time, so the unreachable arm is folded away and only one
/// string survives into the image.
pub const EXTENSION: &str = if cfg!(feature = "compiler") {
    "wren"
} else {
    "wrenc"
};

/// What this build is, for the banner.
pub const FLAVOUR: &str = if cfg!(feature = "compiler") {
    "compiler linked"
} else {
    "no compiler linked"
};

/// The names the node looks for, in the order it runs them.
///
/// MicroPython's order, and for MicroPython's reason: `boot` is the file that
/// is allowed to decide the conditions `main` runs under, so it has to have
/// finished before `main` starts.
pub const SEQUENCE: &[&str] = &["boot", "main"];

/// The bytecode build's store.
#[cfg(not(feature = "compiler"))]
pub const STORE: &[Program] = &[
    Program {
        name: "boot.wrenc",
        bytes: include_bytes!("../../../programs/boot.wrenc"),
    },
    Program {
        name: "main.wrenc",
        bytes: include_bytes!("../../../programs/main.wrenc"),
    },
];

/// The compiler build's store: the same two programs, as source.
#[cfg(feature = "compiler")]
pub const STORE: &[Program] = &[
    Program {
        name: "boot.wren",
        bytes: include_bytes!("../../../programs/boot.wren"),
    },
    Program {
        name: "main.wren",
        bytes: include_bytes!("../../../programs/main.wren"),
    },
];

/// Find the program stored under `stem`, or `None` if there is none.
///
/// Matching on the stem and the extension separately, rather than formatting
/// `"{stem}.{EXTENSION}"` and comparing that, keeps this allocation-free --
/// which matters because it runs before the heap has been proved good.
pub fn find(stem: &str) -> Option<&'static Program> {
    for program in STORE {
        let Some((name, extension)) = program.name.rsplit_once('.') else {
            continue;
        };
        if name == stem && extension == EXTENSION {
            return Some(program);
        }
    }
    None
}
