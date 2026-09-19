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

#[cfg(feature = "std")]
pub fn abs(x: f64) -> f64 {
    x.abs()
}

#[cfg(feature = "std")]
pub fn trunc(x: f64) -> f64 {
    x.trunc()
}

#[cfg(feature = "std")]
pub fn floor(x: f64) -> f64 {
    x.floor()
}

#[cfg(feature = "std")]
pub fn ceil(x: f64) -> f64 {
    x.ceil()
}

#[cfg(feature = "std")]
pub fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

#[cfg(not(feature = "std"))]
pub use fallback::{abs, ceil, floor, sqrt, trunc};

/// The transcendentals, which have to come from somewhere.
///
/// **`std` on a host, `libm` on a target that enables it, and nothing at all
/// otherwise.** Wren's tests check these to fourteen significant digits, which
/// is not something a hand-rolled series reaches reliably -- so rather than
/// ship an approximation that is quietly wrong in the last digits, a build
/// with neither simply does not define the methods. That reports `Num does not
/// implement 'sin'`, which a caller can see, instead of an answer they cannot
/// check.
#[cfg(any(feature = "std", feature = "libm"))]
pub mod real {
    macro_rules! from_std_or_libm {
        ($($name:ident),* $(,)?) => {
            $(
                #[cfg(feature = "std")]
                pub fn $name(x: f64) -> f64 {
                    x.$name()
                }
                #[cfg(all(not(feature = "std"), feature = "libm"))]
                pub fn $name(x: f64) -> f64 {
                    libm::$name(x)
                }
            )*
        };
    }

    from_std_or_libm!(round, exp, log2, cbrt, sin, cos, tan, asin, acos, atan);

    // `ln` is spelled `log` by libm and `ln` by std, and `powf`/`atan2` take
    // two arguments, so these three are written out.
    #[cfg(feature = "std")]
    pub fn ln(x: f64) -> f64 {
        x.ln()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    pub fn ln(x: f64) -> f64 {
        libm::log(x)
    }

    #[cfg(feature = "std")]
    pub fn pow(x: f64, y: f64) -> f64 {
        x.powf(y)
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    pub fn pow(x: f64, y: f64) -> f64 {
        libm::pow(x, y)
    }

    #[cfg(feature = "std")]
    pub fn atan2(y: f64, x: f64) -> f64 {
        y.atan2(x)
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    pub fn atan2(y: f64, x: f64) -> f64 {
        libm::atan2(y, x)
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
