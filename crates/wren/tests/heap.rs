//! The object table and the collector over it.

use wren::heap::Heap;
use wren::object::{MapEntry, ObjClass, ObjList, ObjMap, ObjRange, ObjString};
use wren::{Object, Value};

fn string(heap: &mut Heap, text: &str) -> Value {
    Value::object(heap.allocate(Object::String(ObjString::from_text(text))))
}

fn list(heap: &mut Heap, elements: &[Value]) -> Value {
    let mut object = ObjList::new();
    object.elements.extend_from_slice(elements);
    Value::object(heap.allocate(Object::List(object)))
}

#[test]
fn an_allocated_object_can_be_read_back() {
    let mut heap = Heap::new();
    let handle = heap.allocate(Object::String(ObjString::from_text("hello")));

    match heap.string(handle) {
        Some(text) => assert_eq!(text.as_str(), Some("hello")),
        other => panic!("expected a string, got {other:?}"),
    }
    assert_eq!(heap.live(), 1);
}

#[test]
fn a_stale_handle_reads_as_none_rather_than_as_rubbish() {
    // The property that lets this crate forbid `unsafe`: a handle to a swept
    // object is a failed lookup, not a read of freed memory.
    let mut heap = Heap::new();
    let doomed = heap.allocate(Object::String(ObjString::from_text("gone")));

    heap.collect([]);

    assert!(heap.type_of(doomed).is_none());
    assert_eq!(heap.live(), 0);
}

#[test]
fn a_handle_that_was_never_valid_reads_as_none() {
    let heap = Heap::new();
    assert!(heap.type_of(wren::ObjectId::new(9999)).is_none());
}

#[test]
fn a_root_keeps_its_object() {
    let mut heap = Heap::new();
    let kept = string(&mut heap, "kept");
    let _dropped = string(&mut heap, "dropped");
    assert_eq!(heap.live(), 2);

    let report = heap.collect([kept]);

    assert_eq!(report.before, 2);
    assert_eq!(report.after, 1);
    assert_eq!(report.freed(), 1);
    assert!(heap.type_of(kept.as_object().unwrap()).is_some());
}

#[test]
fn marking_follows_references() {
    // A list root, holding strings that nothing else refers to. If `trace` did
    // not report them, they would be freed while still reachable -- the bug
    // that in a pointer-based collector is a use-after-free.
    let mut heap = Heap::new();
    let first = string(&mut heap, "first");
    let second = string(&mut heap, "second");
    let holder = list(&mut heap, &[first, second]);

    heap.collect([holder]);

    assert_eq!(heap.live(), 3);
    assert!(heap.type_of(first.as_object().unwrap()).is_some());
    assert!(heap.type_of(second.as_object().unwrap()).is_some());
}

#[test]
fn marking_follows_a_deep_chain() {
    // Depth well past anything a recursive mark phase would survive on a part
    // with 8 KB of stack. The work list is a `Vec` for exactly this reason.
    let mut heap = Heap::new();
    let mut current = string(&mut heap, "leaf");
    for _ in 0..10_000 {
        current = list(&mut heap, &[current]);
    }

    let report = heap.collect([current]);

    assert_eq!(report.after, 10_001);
    assert_eq!(report.freed(), 0);
}

#[test]
fn an_unreachable_cycle_is_collected() {
    // **The case that decides mark-sweep against reference counting.** Two
    // lists holding each other, reachable from nothing. A refcount never sees
    // either count reach zero and leaks both; a tracing collector frees them
    // because it starts from the roots and never arrives.
    //
    // Wren builds cycles by construction -- a class refers to its methods, a
    // method's closure refers to its module, the module refers back to the
    // class -- so this is not a contrived case. It is why swapping in a pure
    // refcount would need a cycle collector alongside it.
    let mut heap = Heap::new();
    let left = list(&mut heap, &[]);
    let right = list(&mut heap, &[]);

    match heap.list_mut(left.as_object().unwrap()) {
        Some(object) => object.elements.push(right),
        other => panic!("expected a list, got {other:?}"),
    }
    match heap.list_mut(right.as_object().unwrap()) {
        Some(object) => object.elements.push(left),
        other => panic!("expected a list, got {other:?}"),
    }

    let report = heap.collect([]);

    assert_eq!(report.before, 2);
    assert_eq!(report.after, 0, "an unreachable cycle must not survive");
}

#[test]
fn a_reachable_cycle_survives() {
    // The other half: the same cycle, but rooted. Marking must terminate
    // rather than chase the loop forever.
    let mut heap = Heap::new();
    let left = list(&mut heap, &[]);
    let right = list(&mut heap, &[]);

    match heap.list_mut(left.as_object().unwrap()) {
        Some(object) => object.elements.push(right),
        _ => unreachable!(),
    }
    match heap.list_mut(right.as_object().unwrap()) {
        Some(object) => object.elements.push(left),
        _ => unreachable!(),
    }

    let report = heap.collect([left]);

    assert_eq!(report.after, 2);
}

#[test]
fn an_instance_keeps_its_class_and_fields() {
    let mut heap = Heap::new();
    let name = string(&mut heap, "Point");
    let mut object_class = ObjClass::new(name.as_object().unwrap(), None);
    object_class.num_fields = 2;
    let class = heap.allocate(Object::Class(Box::new(object_class)));
    let field = string(&mut heap, "origin");
    // An instance's fields live in the heap's arena, so the heap is the only
    // thing that can make one.
    let instance = Value::object(heap.new_instance(class, &[field, Value::num(1.0)]));

    heap.collect([instance]);

    // instance, class, the class's name string, and the field's string.
    assert_eq!(heap.live(), 4);
    assert!(heap.type_of(class).is_some());
    assert!(heap.type_of(name.as_object().unwrap()).is_some());
}

#[test]
fn a_map_keeps_its_keys_and_its_values() {
    let mut heap = Heap::new();
    let key = string(&mut heap, "key");
    let value = string(&mut heap, "value");
    let mut map = ObjMap::new();
    map.entries.push(MapEntry { key, value });
    let handle = Value::object(heap.allocate(Object::Map(map)));

    heap.collect([handle]);

    assert_eq!(heap.live(), 3);
}

#[test]
fn freed_slots_are_reused() {
    // Otherwise the table grows without bound in a loop that allocates and
    // drops, which on these parts is the difference between running for a day
    // and running for a minute.
    let mut heap = Heap::new();
    for _ in 0..100 {
        let _ = string(&mut heap, "transient");
        heap.collect([]);
    }
    assert_eq!(heap.live(), 0);

    let kept = string(&mut heap, "kept");
    // One slot, reused a hundred times over. The handle's *index* is what
    // says so -- its low bits name the type's table, so the raw value is not
    // zero even for the first slot.
    assert_eq!(kept.as_object().unwrap().index(), 0);
}

#[test]
fn a_range_holds_no_references() {
    let mut heap = Heap::new();
    let range = Value::object(heap.allocate(Object::Range(ObjRange {
        from: 1.0,
        to: 5.0,
        is_inclusive: true,
    })));

    heap.collect([range]);
    assert_eq!(heap.live(), 1);
}

#[test]
fn collection_is_wanted_only_once_the_threshold_is_passed() {
    let mut heap = Heap::new();
    assert!(
        !heap.should_collect(),
        "a fresh heap has nothing to collect"
    );

    while !heap.should_collect() {
        let _ = string(&mut heap, "filling the heap up with something");
    }

    // And pausing suppresses it, for a construction whose parts are not yet
    // reachable from any root.
    heap.pause();
    assert!(!heap.should_collect());
    heap.resume();
    assert!(heap.should_collect());
}

#[test]
fn the_threshold_follows_the_live_set() {
    let mut heap = Heap::new();
    let mut kept = Vec::new();
    while !heap.should_collect() {
        kept.push(string(&mut heap, "a string that is going to be kept alive"));
    }

    let before = heap.bytes();
    heap.collect(kept.iter().copied());

    // Nothing was garbage, so the live set is unchanged and the next
    // collection has to be further away or the VM would collect continuously.
    assert_eq!(heap.bytes(), before);
    assert!(
        !heap.should_collect(),
        "threshold should have moved past the live set"
    );
    assert_eq!(heap.collections(), 1);
}

#[test]
fn collecting_an_empty_heap_is_harmless() {
    let mut heap = Heap::new();
    let report = heap.collect([]);
    assert_eq!(
        report,
        wren::heap::Collection {
            before: 0,
            after: 0,
            bytes_after: 0
        }
    );
}

#[test]
fn a_tighter_growth_factor_collects_sooner() {
    // Two heaps, the same objects, different thresholds. The one allowed to
    // grow less has to be readier to collect than the one allowed to grow
    // more -- that is the whole of what the dial does, and it is worth a test
    // because getting the fraction upside down would quietly double the
    // memory a firmware uses.
    let mut loose = Heap::new();
    let mut tight = Heap::new();
    tight.set_growth(5, 4);

    for heap in [&mut loose, &mut tight] {
        let mut kept = Vec::new();
        // Enough live bytes that the factor decides the threshold rather
        // than `INITIAL_THRESHOLD`, which is a floor under both.
        for index in 0..512 {
            kept.push(Value::object(heap.allocate(Object::String(
                ObjString::from_text(&format!("string number {index} with some length to it")),
            ))));
        }
        heap.collect(kept.iter().copied());
    }

    assert_eq!(
        loose.bytes(),
        tight.bytes(),
        "same objects, same live bytes"
    );
    assert!(
        tight.threshold() < loose.threshold(),
        "1.25x should collect sooner than 1.5x: {} vs {}",
        tight.threshold(),
        loose.threshold()
    );
}

#[test]
fn a_nonsense_growth_factor_cannot_collect_forever() {
    // A factor at or below 1 would put the threshold at or under the live set,
    // so every allocation would want a collection that frees nothing.
    let mut heap = Heap::new();
    heap.set_growth(1, 2);
    assert!(heap.growth().0 > heap.growth().1, "clamped above 1");
    heap.set_growth(3, 0);
    assert!(heap.growth().1 >= 1, "denominator never zero");
}

#[test]
fn a_ceiling_bounds_garbage_by_a_constant_not_a_ratio() {
    // The property a fixed heap wants: peak is the live set plus a number the
    // firmware chose, whatever the live set turns out to be. The ratio gives
    // an allowance that grows with the workload, which is the wrong shape when
    // the total does not.
    let mut ratio = Heap::new();
    let mut ceiling = Heap::new();
    ceiling.set_headroom(Some(1024));

    for heap in [&mut ratio, &mut ceiling] {
        let mut kept = Vec::new();
        for index in 0..512 {
            kept.push(Value::object(heap.allocate(Object::String(
                ObjString::from_text(&format!("string number {index} with some length to it")),
            ))));
        }
        heap.collect(kept.iter().copied());
    }

    assert_eq!(
        ratio.bytes(),
        ceiling.bytes(),
        "same objects, same live bytes"
    );
    assert_eq!(
        ceiling.threshold(),
        ceiling.bytes() + 1024,
        "a ceiling is the live set plus exactly what was asked for"
    );
    assert!(
        ratio.threshold() > ceiling.threshold(),
        "1.5x of this live set should be further away than 1 KB: {} vs {}",
        ratio.threshold(),
        ceiling.threshold()
    );
}

#[test]
fn a_small_ceiling_is_not_swallowed_by_the_initial_threshold() {
    // `INITIAL_THRESHOLD` is 4 KB, which is half a CH32V006's whole RAM. It
    // exists so a program with three objects does not collect immediately, and
    // it must not quietly override a caller who asked for less.
    let mut heap = Heap::new();
    heap.set_headroom(Some(1024));
    heap.collect([]);
    assert_eq!(heap.threshold(), 1024, "an empty heap plus a 1 KB ceiling");
}

#[test]
fn a_nonsense_ceiling_cannot_collect_forever() {
    // Zero would mean collecting after every allocation, freeing nothing and
    // collecting again.
    let mut heap = Heap::new();
    heap.set_headroom(Some(0));
    assert!(heap.headroom().is_some_and(|bytes| bytes > 0));
    heap.set_headroom(None);
    assert_eq!(heap.headroom(), None);
}

/// **Every slot type still has a spare bit pattern, so `Option` is free.**
///
/// memory.md lists an occupancy bitmap -- one bit per slot, beside the table,
/// instead of an `Option` inside every slot -- as worth building *for a type
/// added later whose payload has no niche*. It is not worth building now,
/// because there is no such type: `Option<T>` is the same size as `T` for all
/// ten, and a bitmap would cost a bit per slot and a test on every access to
/// save nothing.
///
/// `ObjUpvalue` is the one that nearly was not. It holds a stack slot and a
/// closed-over value, and a plain `usize` slot has no spare pattern -- 24
/// bytes for a 16-byte payload, on 19.2% of all allocations. Biasing the slot
/// by one made it a `NonZeroU32` and gave the enum its niche back.
///
/// This fails on the day someone adds a type without one. That is the day the
/// bitmap is worth building, and this is where they will find out.
#[test]
fn every_slot_type_costs_nothing_for_being_optional() {
    macro_rules! niche {
        ($type:ty) => {
            assert_eq!(
                core::mem::size_of::<Option<$type>>(),
                core::mem::size_of::<$type>(),
                concat!(
                    stringify!($type),
                    " has no spare bit pattern, so `Option` costs it a word. ",
                    "Either give it a niche -- see `ObjUpvalue`'s biased slot -- ",
                    "or build the occupancy bitmap that memory.md describes."
                )
            );
        };
    }

    niche!(std::boxed::Box<wren::object::ObjClass>);
    niche!(wren::object::ObjClosure);
    niche!(std::boxed::Box<wren::object::ObjFn>);
    niche!(std::boxed::Box<wren::object::ObjFiber>);
    niche!(wren::object::ObjInstance);
    niche!(wren::object::ObjList);
    niche!(wren::object::ObjMap);
    niche!(wren::object::ObjRange);
    niche!(wren::object::ObjString);
    niche!(wren::object::ObjUpvalue);

    // **The check can fail**, which is worth demonstrating rather than
    // assuming: a payload with no spare bit pattern grows by a word when it
    // is wrapped, and that is exactly what the assertions above would catch.
    assert_ne!(
        core::mem::size_of::<Option<(u32, u64)>>(),
        core::mem::size_of::<(u32, u64)>(),
        "a type with no niche should grow -- if it does not, this test proves nothing"
    );
}
