//! Floating-point helpers with the exact semantics of the C functions the reference encoder calls.
//!
//! `sqrt`, `floor` and `fabs` are exact (correctly rounded) operations, so every backend gives the same bits.
//! The transcendental functions (`log2f`, `atan2`, `cos`, `sin`) are not: with the default `std` feature they
//! go to the platform C runtime through std (on Windows MSVC the UCRT, the same library the C reference links
//! with `/MD`), which is what makes the output byte-identical to the C build. With only the `libm` feature they
//! come from the pure-Rust `libm` crate, which may round differently in the last place.

#[cfg(feature = "std")]
mod imp {
    #[inline]
    pub fn sqrtf(x: f32) -> f32 {
        x.sqrt()
    }
    #[inline]
    pub fn floorf(x: f32) -> f32 {
        x.floor()
    }
    #[inline]
    pub fn floor(x: f64) -> f64 {
        x.floor()
    }
    #[inline]
    pub fn log2f(x: f32) -> f32 {
        x.log2()
    }
    #[inline]
    pub fn atan2(y: f64, x: f64) -> f64 {
        y.atan2(x)
    }
    #[inline]
    pub fn cos(x: f64) -> f64 {
        x.cos()
    }
    #[inline]
    pub fn sin(x: f64) -> f64 {
        x.sin()
    }
}

#[cfg(all(feature = "libm", not(feature = "std")))]
mod imp {
    #[inline]
    pub fn sqrtf(x: f32) -> f32 {
        libm::sqrtf(x)
    }
    #[inline]
    pub fn floorf(x: f32) -> f32 {
        libm::floorf(x)
    }
    #[inline]
    pub fn floor(x: f64) -> f64 {
        libm::floor(x)
    }
    #[inline]
    pub fn log2f(x: f32) -> f32 {
        libm::log2f(x)
    }
    #[inline]
    pub fn atan2(y: f64, x: f64) -> f64 {
        libm::atan2(y, x)
    }
    #[inline]
    pub fn cos(x: f64) -> f64 {
        libm::cos(x)
    }
    #[inline]
    pub fn sin(x: f64) -> f64 {
        libm::sin(x)
    }
}

#[cfg(not(any(feature = "std", feature = "libm")))]
compile_error!("ssc-enc needs a math backend: enable the `std` (default) or the `libm` feature");

#[cfg(any(feature = "std", feature = "libm"))]
pub(crate) use imp::*;

/// `fabsf`: clears the sign bit (exact).
#[inline]
pub(crate) fn fabsf(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0x7fff_ffff)
}

/// C `(int)` conversion of a float as compiled for x86-64 (`cvttss2si`): truncation toward zero, and the
/// "integer indefinite" value `INT_MIN` for NaN or out-of-range inputs (undefined behaviour in C, which MSVC,
/// GCC and Clang all compile to that instruction). Rust's `as i32` would saturate instead.
#[inline]
pub(crate) fn f2i(x: f32) -> i32 {
    if (-2147483648.0..2147483648.0).contains(&x) { x as i32 } else { i32::MIN }
}

/// Same for a double (`cvttsd2si`).
#[inline]
pub(crate) fn d2i(x: f64) -> i32 {
    if x > -2147483649.0 && x < 2147483648.0 { x as i32 } else { i32::MIN }
}

/// `ldexpf(1.f, e)` for the small exponents the encoder uses (exact power of two).
#[inline]
pub(crate) fn exp2i(e: i32) -> f32 {
    debug_assert!((-126..=127).contains(&e));
    f32::from_bits(((127 + e) as u32) << 23)
}
