//! The `no_std` floating-point fallbacks, checked against `std`'s.
//!
//! **This is the test that keeps the device honest.** With `std` the crate uses
//! `std`'s own `sqrt`, `floor` and the rest, so on the host the fallback code
//! is never reached — and the fallback is what a firmware actually ships. So it
//! is called here by name and compared against the implementation it stands in
//! for, across the range and at the edges.

use wren::math::fallback;

/// Values worth checking every function against: the ordinary, the tiny, the
/// enormous, both zeroes, and the boundary where a double stops having a
/// fractional part at all.
fn interesting() -> Vec<f64> {
    let mut values = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -0.5,
        1.5,
        -1.5,
        2.5,
        -2.5,
        3.0,
        -3.0,
        0.1,
        -0.1,
        0.9999999999,
        -0.9999999999,
        1e-300,
        -1e-300,
        1e300,
        -1e300,
        4503599627370496.0, // 2^52, the last value with fractional bits
        4503599627370495.5,
        9007199254740992.0, // 2^53
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::MIN,
    ];
    // A spread through the ordinary range, where most programs live.
    let mut x = -1000.0;
    while x < 1000.0 {
        values.push(x);
        x += 7.3125;
    }
    values
}

#[test]
fn abs_matches_std() {
    for x in interesting() {
        assert_eq!(fallback::abs(x).to_bits(), x.abs().to_bits(), "abs({x})");
    }
    // NaN keeps its payload; the sign is what changes.
    assert!(fallback::abs(f64::NAN).is_nan());
    assert_eq!(fallback::abs(f64::NEG_INFINITY), f64::INFINITY);
}

#[test]
fn trunc_matches_std() {
    for x in interesting() {
        // Compared by bits, so that `-0.0` and `0.0` are not treated as equal:
        // `trunc(-0.5)` must be `-0.0`, and `==` would not catch it if it were
        // not.
        assert_eq!(
            fallback::trunc(x).to_bits(),
            x.trunc().to_bits(),
            "trunc({x}) gave {} want {}",
            fallback::trunc(x),
            x.trunc()
        );
    }
    assert!(fallback::trunc(f64::NAN).is_nan());
    assert_eq!(fallback::trunc(f64::INFINITY), f64::INFINITY);
}

#[test]
fn floor_matches_std() {
    for x in interesting() {
        assert_eq!(
            fallback::floor(x).to_bits(),
            x.floor().to_bits(),
            "floor({x})"
        );
    }
    assert!(fallback::floor(f64::NAN).is_nan());
}

#[test]
fn ceil_matches_std() {
    for x in interesting() {
        assert_eq!(fallback::ceil(x).to_bits(), x.ceil().to_bits(), "ceil({x})");
    }
    assert!(fallback::ceil(f64::NAN).is_nan());
}

#[test]
fn sqrt_matches_std() {
    // Newton's method is not promised to round exactly the way the hardware
    // does, so this asks for agreement to within one unit in the last place
    // rather than bit equality -- and says so rather than quietly using a loose
    // epsilon that would hide a real error.
    for x in interesting() {
        if x < 0.0 {
            assert!(fallback::sqrt(x).is_nan(), "sqrt({x}) should be NaN");
            continue;
        }
        let ours = fallback::sqrt(x);
        let theirs = x.sqrt();
        if theirs == 0.0 || !theirs.is_finite() {
            assert_eq!(ours.to_bits(), theirs.to_bits(), "sqrt({x})");
            continue;
        }
        let ulps = (ours.to_bits() as i64 - theirs.to_bits() as i64).abs();
        assert!(
            ulps <= 1,
            "sqrt({x}) gave {ours}, std gave {theirs}, {ulps} ulps apart"
        );
    }
}

#[test]
fn sqrt_of_perfect_squares_is_exact() {
    // The case a program is most likely to notice: `9.sqrt` printing
    // `2.9999999999999996` would be visible in output and wrong.
    for root in 0..1000 {
        let root = root as f64;
        let square = root * root;
        assert_eq!(fallback::sqrt(square), root, "sqrt({square})");
    }
}

#[test]
fn sqrt_edges() {
    assert!(fallback::sqrt(f64::NAN).is_nan());
    assert_eq!(fallback::sqrt(f64::INFINITY), f64::INFINITY);
    assert_eq!(fallback::sqrt(0.0).to_bits(), 0.0f64.to_bits());
    assert_eq!(fallback::sqrt(-0.0).to_bits(), (-0.0f64).to_bits());
    assert!(fallback::sqrt(-1.0).is_nan());
}
