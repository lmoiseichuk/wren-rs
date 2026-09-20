//! What the build has to be told to watch.
//!
//! **Cargo does not rebuild for an environment variable it was never told
//! about.** `option_env!` reads one at compile time, so without this line a
//! sweep over block sizes would flash the same image every round and report
//! one number four times. There is nothing else here: the linker script comes
//! from `.cargo/config.toml`, not from a build script.
fn main() {
    println!("cargo::rerun-if-env-changed=WREN_SLOT_BLOCK");
    println!("cargo::rerun-if-env-changed=WREN_REPEATS");
}
