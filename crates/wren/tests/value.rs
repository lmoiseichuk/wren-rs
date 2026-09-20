//! The value representation, at full width.
//!
//! NaN tagging is the kind of trick that works for years and then fails on one
//! input, so the interesting tests here are the edges: a genuine NaN arriving
//! from arithmetic, the extremes of the handle range, and every singleton
//! against every other.
//!
//! **These assert the 64-bit layout specifically** — eight bytes, 53 bits of
//! integer, a 32-bit handle — so the whole file is skipped in an `f32` build,
//! which has its own in `value_f32.rs`. Making them width-generic would have
//! meant asserting whichever answer the build gives, which is not a test.
#![cfg(all(not(feature = "f32"), not(feature = "nofp")))]

use wren::object::ObjectId;
use wren::Value;

#[test]
fn numbers_survive_a_round_trip() {
    for number in [
        0.0,
        -0.0,
        1.0,
        -1.0,
        core::f64::consts::PI,
        1e300,
        -1e300,
        1e-300,
    ] {
        let value = Value::num(number);
        assert!(value.is_num(), "{number} should be a number");
        assert_eq!(value.as_num(), Some(number));
        assert!(!value.is_object());
        assert!(!value.is_null());
    }
}

#[test]
fn integers_are_exact_to_fifty_three_bits() {
    // Wren has one numeric type and programs rely on integers being exact
    // within a double's mantissa. This is the boundary.
    let largest_exact = 9_007_199_254_740_992f64; // 2^53
    assert_eq!(Value::num(largest_exact).as_num(), Some(largest_exact));
    assert_eq!(Value::num(-largest_exact).as_num(), Some(-largest_exact));
}

#[test]
fn infinities_are_numbers() {
    for number in [f64::INFINITY, f64::NEG_INFINITY] {
        let value = Value::num(number);
        assert!(value.is_num());
        assert_eq!(value.as_num(), Some(number));
    }
}

#[test]
fn a_real_nan_is_a_number_and_not_a_tag() {
    // **The failure mode NaN tagging exists to survive.** A NaN produced by
    // arithmetic must not be mistaken for null, for a boolean, or for an object
    // handle. If the quiet-NaN constant were chosen wrongly this is the test
    // that would catch it.
    let zero = core::hint::black_box(0.0f64);
    let produced = zero / zero;
    assert!(produced.is_nan());

    let value = Value::num(produced);
    assert!(value.is_num());
    assert!(value.as_num().unwrap().is_nan());
    assert!(!value.is_null());
    assert!(!value.is_bool());
    assert!(!value.is_object());
    assert!(!value.is_undefined());
}

#[test]
fn every_flavour_of_nan_stays_a_number() {
    // Sweep the payload space rather than trusting one example: any bit pattern
    // with the exponent saturated and a non-zero mantissa is a NaN, and every
    // one of them has to read back as a number.
    for shift in 0..51 {
        let bits = 0x7ff0_0000_0000_0000u64 | (1u64 << shift);
        let number = f64::from_bits(bits);
        assert!(number.is_nan(), "0x{bits:016x} should be NaN");
        assert!(
            Value::num(number).is_num(),
            "0x{bits:016x} should read as a number"
        );
    }
}

#[test]
fn singletons_are_distinct() {
    let all = [Value::NULL, Value::TRUE, Value::FALSE, Value::UNDEFINED];
    for (at, one) in all.iter().enumerate() {
        for (also, other) in all.iter().enumerate() {
            assert_eq!(one.is_same(*other), at == also, "{one:?} vs {other:?}");
        }
        assert!(!one.is_num(), "{one:?} should not be a number");
        assert!(!one.is_object(), "{one:?} should not be an object");
    }
}

#[test]
fn booleans_map_to_singletons() {
    assert!(Value::bool(true).is_same(Value::TRUE));
    assert!(Value::bool(false).is_same(Value::FALSE));
    assert!(Value::TRUE.is_bool());
    assert!(Value::FALSE.is_bool());
    assert!(!Value::NULL.is_bool());
}

#[test]
fn only_false_and_null_are_falsy() {
    // Wren's rule, and it catches people out: zero is true, the empty string is
    // true. Anything that changes this changes the language.
    assert!(Value::FALSE.is_falsy());
    assert!(Value::NULL.is_falsy());

    assert!(!Value::TRUE.is_falsy());
    assert!(!Value::num(0.0).is_falsy());
    assert!(!Value::num(-0.0).is_falsy());
    assert!(!Value::num(f64::NAN).is_falsy());
    assert!(!Value::object(ObjectId::new(0)).is_falsy());
}

#[test]
fn object_handles_survive_a_round_trip() {
    // Including both ends of the range: handle 0 is the first object ever
    // allocated, and u32::MAX proves the payload is not being truncated.
    for raw in [0u32, 1, 255, 65535, 1 << 24, u32::MAX - 1, u32::MAX] {
        let value = Value::object(ObjectId::new(raw));
        assert!(value.is_object(), "handle {raw} should be an object");
        assert!(!value.is_num());
        assert_eq!(value.as_object().map(ObjectId::raw), Some(raw));
    }
}

#[test]
fn wrong_type_accessors_return_none() {
    assert_eq!(Value::NULL.as_num(), None);
    assert_eq!(Value::TRUE.as_num(), None);
    assert_eq!(Value::object(ObjectId::new(7)).as_num(), None);
    assert_eq!(Value::num(1.0).as_object(), None);
    assert_eq!(Value::NULL.as_object(), None);
}

#[test]
fn a_value_is_eight_bytes() {
    // The whole reason for NaN tagging. A tagged enum would be sixteen, and
    // every stack slot, list element and instance field pays the difference.
    assert_eq!(core::mem::size_of::<Value>(), 8);
}

#[test]
fn bits_round_trip_for_a_bytecode_writer() {
    for value in [
        Value::num(42.5),
        Value::NULL,
        Value::TRUE,
        Value::FALSE,
        Value::object(ObjectId::new(9)),
    ] {
        assert!(Value::from_bits(value.to_bits()).is_same(value));
    }
}

#[test]
fn debug_says_what_it_is() {
    assert_eq!(format!("{:?}", Value::num(1.5)), "Num(1.5)");
    assert_eq!(format!("{:?}", Value::NULL), "Null");
    assert_eq!(format!("{:?}", Value::TRUE), "True");
    assert_eq!(format!("{:?}", Value::FALSE), "False");
    assert_eq!(format!("{:?}", Value::UNDEFINED), "Undefined");
    assert_eq!(
        format!("{:?}", Value::object(ObjectId::new(3))),
        "Object(3)"
    );
}
