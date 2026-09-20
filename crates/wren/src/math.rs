//! The floating-point operations `core` does not provide.
//!
//! `f64::floor`, `ceil`, `trunc`, `abs` and `sqrt` live in `std`, not in
//! `core`, because they are normally provided by the platform's libm.
//!
//! **With `std`, these are `std`'s** — which on a host means the hardware
//! instruction, correctly rounded, and no reason to prefer anything else.
//! Without it, a firmware has no libm and this crate has no dependencies, so
//! the [`fallback`] module provides the handful that Wren's `Num` class needs.
//!
//! The risk in having two implementations is that the one which actually ships
//! is the one never exercised, since the tests run on the host. That is worth
//! naming, and it is why the fallback is a public module rather than a private
//! `cfg` branch: the test suite calls it **by name** and checks it against
//! `std`'s across the range, so the device path is tested on every run.

use crate::value::Num;

// **An integer is already whole**, so rounding it is the identity and the
// four shaping functions cost nothing. They stay defined rather than being
// removed because the shared code -- indexing, iteration, range walking --
// calls them on values that are conceptually numbers, not floats, and would
// otherwise need a `cfg` at every call site.
#[cfg(feature = "no-fp")]
pub fn abs(x: Num) -> Num {
    x.wrapping_abs()
}

#[cfg(feature = "no-fp")]
pub fn trunc(x: Num) -> Num {
    x
}

#[cfg(feature = "no-fp")]
pub fn floor(x: Num) -> Num {
    x
}

#[cfg(feature = "no-fp")]
pub fn ceil(x: Num) -> Num {
    x
}

/// Integer square root by Newton's method, which needs no floating point.
#[cfg(feature = "no-fp")]
pub fn sqrt(x: Num) -> Num {
    if x <= 0 {
        return 0;
    }
    let mut guess = x;
    let mut next = (guess + 1) / 2;
    while next < guess {
        guess = next;
        next = (guess + x / guess) / 2;
    }
    guess
}

// `std`'s methods exist on both `f32` and `f64`, so one definition covers
// either width.
#[cfg(all(feature = "std", not(feature = "no-fp")))]
pub fn abs(x: Num) -> Num {
    x.abs()
}

#[cfg(all(feature = "std", not(feature = "no-fp")))]
pub fn trunc(x: Num) -> Num {
    x.trunc()
}

#[cfg(all(feature = "std", not(feature = "no-fp")))]
pub fn floor(x: Num) -> Num {
    x.floor()
}

#[cfg(all(feature = "std", not(feature = "no-fp")))]
pub fn ceil(x: Num) -> Num {
    x.ceil()
}

#[cfg(all(feature = "std", not(feature = "no-fp")))]
pub fn sqrt(x: Num) -> Num {
    x.sqrt()
}

// **The fallbacks stay `f64` and a narrower build widens into them.** One
// implementation, one set of tests, and the answers are the same ones: `abs`,
// `trunc`, `floor` and `ceil` are exact under widening, and `f64` has more
// than twice `f32`'s mantissa plus two bits, which is the condition under
// which rounding a `f64` square root down to `f32` gives the correctly rounded
// `f32` result. Writing a second bit-twiddling implementation to save a
// conversion on the path that has no libm would be trading a tested thing for
// an untested one.
#[cfg(all(not(feature = "std"), not(feature = "no-fp")))]
pub fn abs(x: Num) -> Num {
    fallback::abs(x as f64) as Num
}

#[cfg(all(not(feature = "std"), not(feature = "no-fp")))]
pub fn trunc(x: Num) -> Num {
    fallback::trunc(x as f64) as Num
}

#[cfg(all(not(feature = "std"), not(feature = "no-fp")))]
pub fn floor(x: Num) -> Num {
    fallback::floor(x as f64) as Num
}

#[cfg(all(not(feature = "std"), not(feature = "no-fp")))]
pub fn ceil(x: Num) -> Num {
    fallback::ceil(x as f64) as Num
}

#[cfg(all(not(feature = "std"), not(feature = "no-fp")))]
pub fn sqrt(x: Num) -> Num {
    fallback::sqrt(x as f64) as Num
}

/// The transcendentals, which have to come from somewhere.
///
/// **`std` on a host, `libm` on a target that enables it, and nothing at all
/// otherwise.** Wren's tests check these to fourteen significant digits, which
/// is not something a hand-rolled series reaches reliably -- so rather than
/// ship an approximation that is quietly wrong in the last digits, a build
/// with neither simply does not define the methods. That reports `Num does not
/// implement 'sin'`, which a caller can see, instead of an answer they cannot
/// check.
#[cfg(all(any(feature = "std", feature = "libm"), not(feature = "no-fp")))]
pub mod real {
    use crate::value::Num;

    // **libm names its `f32` entry points with an `f` suffix**, so a narrow
    // build names them explicitly rather than widening to `f64` and back. That
    // matters here in a way it does not for the five above: these are the
    // expensive ones, and on a part with no FPU a software `f64` sine is
    // several times the cost of a software `f32` one.
    macro_rules! from_std_or_libm {
        ($($name:ident / $narrow:ident),* $(,)?) => {
            $(
                #[cfg(feature = "std")]
                pub fn $name(x: Num) -> Num {
                    x.$name()
                }
                #[cfg(all(not(feature = "std"), feature = "libm", not(feature = "f32")))]
                pub fn $name(x: Num) -> Num {
                    libm::$name(x)
                }
                #[cfg(all(not(feature = "std"), feature = "libm", feature = "f32"))]
                pub fn $name(x: Num) -> Num {
                    libm::$narrow(x)
                }
            )*
        };
    }

    from_std_or_libm!(
        round / roundf,
        exp / expf,
        log2 / log2f,
        cbrt / cbrtf,
        sin / sinf,
        cos / cosf,
        tan / tanf,
        asin / asinf,
        acos / acosf,
        atan / atanf,
    );

    // `ln` is spelled `log` by libm and `ln` by std, and `pow`/`atan2` take
    // two arguments, so these three are written out.
    #[cfg(feature = "std")]
    pub fn ln(x: Num) -> Num {
        x.ln()
    }
    #[cfg(all(not(feature = "std"), feature = "libm", not(feature = "f32")))]
    pub fn ln(x: Num) -> Num {
        libm::log(x)
    }
    #[cfg(all(not(feature = "std"), feature = "libm", feature = "f32"))]
    pub fn ln(x: Num) -> Num {
        libm::logf(x)
    }

    #[cfg(feature = "std")]
    pub fn pow(x: Num, y: Num) -> Num {
        x.powf(y)
    }
    #[cfg(all(not(feature = "std"), feature = "libm", not(feature = "f32")))]
    pub fn pow(x: Num, y: Num) -> Num {
        libm::pow(x, y)
    }
    #[cfg(all(not(feature = "std"), feature = "libm", feature = "f32"))]
    pub fn pow(x: Num, y: Num) -> Num {
        libm::powf(x, y)
    }

    #[cfg(feature = "std")]
    pub fn atan2(y: Num, x: Num) -> Num {
        y.atan2(x)
    }
    #[cfg(all(not(feature = "std"), feature = "libm", not(feature = "f32")))]
    pub fn atan2(y: Num, x: Num) -> Num {
        libm::atan2(y, x)
    }
    #[cfg(all(not(feature = "std"), feature = "libm", feature = "f32"))]
    pub fn atan2(y: Num, x: Num) -> Num {
        libm::atan2f(y, x)
    }
}

/// The `no_std` implementations.
///
/// Public so the tests can compare them against `std`'s even in a build where
/// `std` is what gets used. Nothing else should call these directly — use the
/// module's own [`sqrt`] and friends, which pick the right one.
pub mod fallback {
    /// `|x|`, by clearing the sign bit.
    ///
    /// Works for every input including NaN and the infinities, and unlike a
    /// comparison against zero it gets `-0.0` right.
    pub fn abs(x: f64) -> f64 {
        f64::from_bits(x.to_bits() & !(1 << 63))
    }

    /// Round toward zero.
    ///
    /// Done by masking off the fractional bits rather than by casting through an
    /// integer: a cast is undefined for values outside the integer's range, and
    /// `1e300` is a perfectly ordinary Wren number.
    pub fn trunc(x: f64) -> f64 {
        if !x.is_finite() || x == 0.0 {
            return x;
        }
        let bits = x.to_bits();
        let exponent = ((bits >> 52) & 0x7ff) as i32 - 1023;

        if exponent < 0 {
            // |x| < 1, so the whole value is fraction. The sign is kept, which is
            // what makes `trunc(-0.5)` produce `-0.0` rather than `0.0`.
            return f64::from_bits(bits & (1 << 63));
        }
        if exponent >= 52 {
            // No fractional bits left to clear; the value is already integral.
            return x;
        }
        let mask = (1u64 << (52 - exponent)) - 1;
        f64::from_bits(bits & !mask)
    }

    /// Round toward negative infinity.
    pub fn floor(x: f64) -> f64 {
        let truncated = trunc(x);
        if x < 0.0 && truncated != x {
            return truncated - 1.0;
        }
        truncated
    }

    /// Round toward positive infinity.
    pub fn ceil(x: f64) -> f64 {
        let truncated = trunc(x);
        if x > 0.0 && truncated != x {
            return truncated + 1.0;
        }
        truncated
    }

    /// Square root, by Newton's method.
    ///
    /// The initial guess comes from halving the exponent in the bit pattern, which
    /// lands within a factor of about 1.4. Newton doubles the number of correct
    /// bits each step, so five steps take that to well past a double's 53 — and a
    /// sixth is there because the last step is what makes the result round
    /// correctly rather than merely closely. The tests check it against `std`'s
    /// across the range.
    pub fn sqrt(x: f64) -> f64 {
        if x.is_nan() || x < 0.0 {
            return f64::NAN;
        }
        // Zero returns itself so that `sqrt(-0.0)` is `-0.0`, and infinity has no
        // finite guess to start from.
        if x == 0.0 || x == f64::INFINITY {
            return x;
        }

        // Halving the exponent: adding the bias before shifting keeps the result
        // biased correctly afterwards.
        let mut guess = f64::from_bits((x.to_bits() + (1023u64 << 52)) >> 1);

        let mut step = 0;
        while step < 6 {
            guess = 0.5 * (guess + x / guess);
            step += 1;
        }
        guess
    }
}
