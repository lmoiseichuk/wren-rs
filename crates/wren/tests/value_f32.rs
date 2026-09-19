//! The value representation at 32 bits.
//!
//! The mirror of `value.rs` for the `f32` feature, and it is a separate file
//! rather than a set of `cfg` arms inside that one because the *answers*
//! differ, not just the types: four bytes instead of eight, 24 bits of exact
//! integer instead of 53, and a handle that has 21 bits rather than 32.
//!
//! **The 21-bit handle is the only thing this build genuinely gives up**, so
//! it is what most of these are about. A single-precision quiet NaN leaves the
//! sign bit and 23 mantissa bits; `QNAN` spends two of those, and the rest is
//! the payload.

#![cfg(feature = "f32")]

use wren::object::ObjectId;
use wren::Value;

/// The largest handle this layout can carry. See `PAYLOAD` in `value.rs`.
const LARGEST_HANDLE: u32 = (1 << 21) - 1;

#[test]
fn numbers_survive_a_round_trip() {
    for number in [0.0f32, -0.0, 1.0, -1.0, 0.5, 1.2345679, 1e30, -1e30, 1e-30] {
        let value = Value::num(number);
        assert!(value.is_num(), "{number} should be a number");
        assert_eq!(value.as_num(), Some(number));
    }
}

#[test]
fn integers_are_exact_to_twenty_four_bits() {
    // An `f32` has a 24-bit significand, so this is the largest integer it can
    // represent and the one above it is not representable at all. That is the
    // limit the feature's documentation warns about, stated as a test.
    let largest_exact = (1u32 << 24) as f32;
    assert_eq!(Value::num(largest_exact).as_num(), Some(largest_exact));
    assert_eq!(
        largest_exact + 1.0,
        largest_exact,
        "16,777,217 is not representable"
    );
}

#[test]
fn infinities_are_numbers() {
    for number in [f32::INFINITY, f32::NEG_INFINITY] {
        let value = Value::num(number);
        assert!(value.is_num());
        assert!(!value.is_object());
        assert_eq!(value.as_num(), Some(number));
    }
}

#[test]
fn a_real_nan_is_a_number_and_not_a_tag() {
    // The NaN arithmetic actually produces, rather than one written down: this
    // is the case the extra quiet bit exists to keep apart from a tag.
    let produced = f32::INFINITY - f32::INFINITY;
    assert!(produced.is_nan());

    let value = Value::num(produced);
    assert!(value.is_num(), "a real NaN must still be a number");
    assert!(!value.is_null() && !value.is_true() && !value.is_false());
    assert!(!value.is_undefined() && !value.is_object());
    assert!(value.as_num().is_some_and(f32::is_nan));
}

#[test]
fn singletons_are_distinct() {
    let all = [Value::NULL, Value::TRUE, Value::FALSE, Value::UNDEFINED];
    for (index, one) in all.iter().enumerate() {
        for (other_index, other) in all.iter().enumerate() {
            assert_eq!(
                one.is_same(*other),
                index == other_index,
                "singleton {index} against {other_index}"
            );
        }
        assert!(!one.is_num(), "a singleton is not a number");
        assert!(!one.is_object(), "a singleton is not a handle");
    }
}

#[test]
fn object_handles_survive_a_round_trip() {
    // Both ends of what this layout can hold. Handle 0 is the first object
    // ever allocated; `LARGEST_HANDLE` proves the payload reaches the top of
    // the 21 bits and is not being truncated on the way back out.
    for raw in [
        0u32,
        1,
        255,
        65535,
        1 << 20,
        LARGEST_HANDLE - 1,
        LARGEST_HANDLE,
    ] {
        let value = Value::object(ObjectId::new(raw));
        assert!(value.is_object(), "handle {raw} should be an object");
        assert!(!value.is_num());
        assert_eq!(value.as_object().map(ObjectId::raw), Some(raw));
    }
}

#[test]
fn a_value_is_four_bytes() {
    // The whole memory argument for this feature. If this ever grows, every
    // stack slot, list element, instance field and map entry grew with it.
    assert_eq!(core::mem::size_of::<Value>(), 4);
}

#[test]
fn bits_round_trip_for_a_bytecode_writer() {
    for value in [
        Value::num(1.5),
        Value::NULL,
        Value::TRUE,
        Value::FALSE,
        Value::object(ObjectId::new(LARGEST_HANDLE)),
    ] {
        assert!(Value::from_bits(value.to_bits()).is_same(value));
    }
}

#[test]
fn only_false_and_null_are_falsy() {
    assert!(Value::FALSE.is_falsy());
    assert!(Value::NULL.is_falsy());
    assert!(!Value::TRUE.is_falsy());
    assert!(!Value::num(0.0).is_falsy(), "zero is true in Wren");
    assert!(!Value::object(ObjectId::new(1)).is_falsy());
}
