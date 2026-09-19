//! The object table and the collector over it.

use wren::heap::Heap;
use wren::object::{MapEntry, ObjClass, ObjInstance, ObjList, ObjMap, ObjRange, ObjString};
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

    match heap.get(handle) {
        Some(Object::String(text)) => assert_eq!(text.as_str(), Some("hello")),
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

    assert!(heap.get(doomed).is_none());
    assert_eq!(heap.live(), 0);
}

#[test]
fn a_handle_that_was_never_valid_reads_as_none() {
    let heap = Heap::new();
    assert!(heap.get(wren::ObjectId::new(9999)).is_none());
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
    assert!(heap.get(kept.as_object().unwrap()).is_some());
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
    assert!(heap.get(first.as_object().unwrap()).is_some());
    assert!(heap.get(second.as_object().unwrap()).is_some());
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

    match heap.get_mut(left.as_object().unwrap()) {
        Some(Object::List(object)) => object.elements.push(right),
        other => panic!("expected a list, got {other:?}"),
    }
    match heap.get_mut(right.as_object().unwrap()) {
        Some(Object::List(object)) => object.elements.push(left),
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

    match heap.get_mut(left.as_object().unwrap()) {
        Some(Object::List(object)) => object.elements.push(right),
        _ => unreachable!(),
    }
    match heap.get_mut(right.as_object().unwrap()) {
        Some(Object::List(object)) => object.elements.push(left),
        _ => unreachable!(),
    }

    let report = heap.collect([left]);

    assert_eq!(report.after, 2);
}

#[test]
fn an_instance_keeps_its_class_and_fields() {
    let mut heap = Heap::new();
    let name = string(&mut heap, "Point");
    let class = heap.allocate(Object::Class(ObjClass {
        name: name.as_object().unwrap(),
        superclass: None,
        num_fields: 2,
    }));
    let field = string(&mut heap, "origin");
    let instance = Value::object(heap.allocate(Object::Instance(ObjInstance {
        class,
        fields: alloc_fields(&[field, Value::num(1.0)]),
    })));

    heap.collect([instance]);

    // instance, class, the class's name string, and the field's string.
    assert_eq!(heap.live(), 4);
    assert!(heap.get(class).is_some());
    assert!(heap.get(name.as_object().unwrap()).is_some());
}

fn alloc_fields(values: &[Value]) -> Vec<Value> {
    values.to_vec()
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
    // One slot, reused a hundred times over.
    assert_eq!(kept.as_object().unwrap().raw(), 0);
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
    assert!(!heap.should_collect(), "a fresh heap has nothing to collect");

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
    assert!(!heap.should_collect(), "threshold should have moved past the live set");
    assert_eq!(heap.collections(), 1);
}

#[test]
fn collecting_an_empty_heap_is_harmless() {
    let mut heap = Heap::new();
    let report = heap.collect([]);
    assert_eq!(report, wren::heap::Collection { before: 0, after: 0, bytes_after: 0 });
}
