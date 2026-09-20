//! The value representation without floating point.
//!
//! The mirror of `value.rs` for the `no-fp` feature, and a separate file for
//! the same reason that one is: the *answers* differ. There is no NaN to box
//! against, so a number is tagged by its low bit, a handle by the sign bit,
//! and every singleton is an even word with neither — which is what makes the
//! three kinds impossible to read as each other.
#![cfg(feature = "no-fp")]

use wren::value::Num;
use wren::{ObjectId, Value};

#[test]
fn a_number_survives_the_round_trip() {
    for number in [0, 1, -1, 2, -2, 1000, -1000, 1_073_741_823, -1_073_741_824] {
        let value = Value::num(number);
        assert!(value.is_num(), "{number} should be a number");
        assert_eq!(value.as_num(), Some(number), "{number} should read back");
        assert!(!value.is_object(), "{number} is not a handle");
        assert!(!value.is_null() && !value.is_bool(), "{number} is not a singleton");
    }
}

#[test]
fn the_range_is_thirty_one_bits_because_the_tag_takes_one() {
    // `NUM_MAX` is the largest value, and one past it is where wrapping
    // starts. Stated as a test because it is the one thing a program written
    // for the `f64` build would notice first.
    let largest: Num = wren::value::NUM_MAX;
    assert_eq!(largest, 1_073_741_823);
    assert_eq!(Value::num(largest).as_num(), Some(largest));
    let smallest: Num = -largest - 1;
    assert_eq!(Value::num(smallest).as_num(), Some(smallest));
}

#[test]
fn a_handle_is_not_a_number_and_not_a_singleton() {
    for raw in [0u32, 1, 2, 255, 65_535, 1_073_741_823] {
        let value = Value::object(ObjectId::new(raw));
        assert!(value.is_object(), "handle {raw} should be a handle");
        assert_eq!(value.as_object().map(|id| id.raw()), Some(raw));
        assert!(!value.is_num(), "handle {raw} must not read as a number");
        assert!(value.as_num().is_none(), "handle {raw} has no numeric value");
    }
}

#[test]
fn every_singleton_is_distinct_from_every_other_and_from_numbers() {
    let singletons = [Value::NULL, Value::TRUE, Value::FALSE];
    for (index, one) in singletons.iter().enumerate() {
        assert!(!one.is_num(), "singleton {index} must not read as a number");
        assert!(!one.is_object(), "singleton {index} must not read as a handle");
        for (other, two) in singletons.iter().enumerate() {
            assert_eq!(
                one.is_same(*two),
                index == other,
                "singleton {index} against {other}"
            );
        }
    }
    assert!(Value::NULL.is_null());
    assert!(Value::TRUE.is_true() && Value::FALSE.is_false());
}

#[test]
fn a_value_is_one_word() {
    // The whole point of tagging rather than an enum: four bytes, so a stack
    // slot, a list element and an instance field each cost one word.
    assert_eq!(core::mem::size_of::<Value>(), 4);
}
