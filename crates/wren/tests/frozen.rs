//! A core library compiled into the image, checked against the one built in RAM.
//!
//! **This is the test that makes the generator safe to iterate on**, and it
//! runs on the host: a flash-and-watch cycle on the part is the better part of
//! a minute, and the interesting failures here -- a symbol numbered
//! differently, a method table indexed against the wrong install order -- are
//! not failures a device would report as anything but a wrong answer.
//!
//! Regenerate the module under test with:
//!
//!     cargo run --example freeze -- benchmarks/wrenc/fib.wrenc \
//!         > crates/wren/tests/generated/fib_core.rs

include!("generated/fib_core.rs");

const FIB: &[u8] = include_bytes!("../../../benchmarks/wrenc/fib.wrenc");

fn manifest() -> wren::wrenc::Manifest {
    wren::wrenc::manifest(FIB).expect("fib.wrenc should be loadable")
}

/// The generated symbols are the ones this build of the crate interns.
///
/// **The failure this catches is silent.** A frozen method table is a list of
/// symbol numbers; a core generated before a `define` was added or moved would
/// index the right table with the wrong numbers, call the wrong primitive, and
/// report nothing. Any change to the set or order of `define` calls changes
/// this list, so comparing it is what turns that into a test failure.
#[test]
fn the_frozen_core_agrees_with_this_build() {
    let vm = wren::Vm::with_manifest(&manifest());
    match CORE.disagreement(&vm.method_names, vm.primitives.len()) {
        None => {}
        Some(why) => panic!("the generated core is stale: {why}\n\nregenerate it: cargo run --example freeze -- benchmarks/wrenc/fib.wrenc > crates/wren/tests/generated/fib_core.rs"),
    }
}

/// A VM built from the image runs the program, and gets the same answer.
#[test]
fn a_frozen_core_runs_the_program() {
    let manifest = manifest();
    let mut vm = wren::Vm::with_frozen_core(&CORE, &manifest);
    let loaded = wren::wrenc::load(&mut vm, FIB).expect("fib.wrenc should load");
    vm.run_closure(loaded.closure).expect("fib should run");

    let output = vm.output_str();
    let answer = output
        .lines()
        .find(|line| !line.starts_with("elapsed:"))
        .unwrap_or("(nothing)");
    assert_eq!(answer, "46368");
}

/// It gets the same answer as a VM that built its core in RAM.
///
/// Compared rather than asserted against a literal, so this keeps testing the
/// two paths against each other if `fib.wrenc` is ever rebuilt.
#[test]
fn frozen_and_built_cores_agree_on_the_output() {
    let manifest = manifest();

    let mut built = wren::Vm::with_manifest(&manifest);
    let loaded = wren::wrenc::load(&mut built, FIB).expect("load");
    built.run_closure(loaded.closure).expect("run");
    let from_ram: Vec<&str> = built.output_str().lines().collect();

    let mut frozen = wren::Vm::with_frozen_core(&CORE, &manifest);
    let loaded = wren::wrenc::load(&mut frozen, FIB).expect("load");
    frozen.run_closure(loaded.closure).expect("run");
    let from_flash: Vec<&str> = frozen.output_str().lines().collect();

    // The elapsed line is a clock reading and will not match.
    let strip = |lines: &Vec<&str>| -> Vec<String> {
        lines
            .iter()
            .filter(|line| !line.starts_with("elapsed:"))
            .map(|line| line.to_string())
            .collect()
    };
    assert_eq!(strip(&from_ram), strip(&from_flash));
}

/// Every class is addressable, named, and refuses to be mutated.
///
/// The three properties `install` depends on: it looks classes up by handle,
/// reports errors by name, and must find every `define` into one a no-op.
#[test]
fn every_frozen_class_is_addressable_and_refused_for_writing() {
    use wren::handle::ObjectId;
    use wren::object::ObjectType;

    let manifest = manifest();
    let mut vm = wren::Vm::with_frozen_core(&CORE, &manifest);

    for index in 0..CORE.classes.len() as u32 {
        let id = ObjectId::tagged(ObjectType::Class.tag(), index);
        assert!(
            vm.heap.class(id).is_some(),
            "frozen class {index} should be reachable"
        );
        assert!(
            vm.heap.class_is_static(id),
            "class {index} should be in the image, not the heap"
        );
        assert!(
            vm.heap.class_mut(id).is_none(),
            "frozen class {index} must refuse mutation"
        );

        // Its name is the string at its own index, which is the invariant the
        // generated handles were emitted against.
        let name = vm.heap.class(id).unwrap().name;
        assert_eq!(name.index(), index, "class {index} should be named by string {index}");
        assert!(
            vm.heap.string(name).is_some(),
            "class {index}'s name string should exist"
        );
    }
}

/// The classes cost the heap nothing, which is the whole point.
#[test]
fn a_frozen_core_is_not_charged_to_the_heap() {
    let manifest = manifest();
    let built = wren::Vm::with_manifest(&manifest);
    let frozen = wren::Vm::with_frozen_core(&CORE, &manifest);

    let saved = built.heap.bytes().saturating_sub(frozen.heap.bytes());
    assert!(
        saved > 1_000,
        "freezing {} classes should save more than a kilobyte, saved {saved} B \
         (built {} B, frozen {} B)",
        CORE.classes.len(),
        built.heap.bytes(),
        frozen.heap.bytes()
    );
}
