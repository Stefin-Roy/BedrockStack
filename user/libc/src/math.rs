//! `<math.h>` surface backed by the permissive no_std `libm` crate.
//!
//! Only the double-precision entry points are exported for the common
//! porting surface; float variants delegate through `libm`'s `*f` forms.

use core::ffi::{c_char, c_double, c_float, c_int, c_long, c_longlong};

macro_rules! d {
    ($name:ident, $lm:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(x: c_double) -> c_double {
            libm::$lm(x)
        }
    };
    ($name:ident, $lm:ident, f $fm:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(x: c_double) -> c_double {
            libm::$lm(x)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn $fm(x: c_float) -> c_float {
            libm::$fm(x)
        }
    };
}

/// Binary (two-argument) double functions.
macro_rules! d2 {
    ($name:ident, $lm:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(x: c_double, y: c_double) -> c_double {
            libm::$lm(x, y)
        }
    };
    ($name:ident, $lm:ident, f $fm:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(x: c_double, y: c_double) -> c_double {
            libm::$lm(x, y)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn $fm(x: c_float, y: c_float) -> c_float {
            libm::$fm(x, y)
        }
    };
}

d!(fabs, fabs, f fabsf);
d!(sin, sin, f sinf);
d!(cos, cos, f cosf);
d!(tan, tan, f tanf);
d!(asin, asin, f asinf);
d!(acos, acos, f acosf);
d!(atan, atan, f atanf);
d!(sinh, sinh, f sinhf);
d!(cosh, cosh, f coshf);
d!(tanh, tanh, f tanhf);
d!(sqrt, sqrt, f sqrtf);
d!(cbrt, cbrt, f cbrtf);
d!(exp, exp, f expf);
d!(exp2, exp2, f exp2f);
d!(expm1, expm1, f expm1f);
d!(log, log, f logf);
d!(log10, log10, f log10f);
d!(log2, log2, f log2f);
d!(log1p, log1p, f log1pf);
d!(floor, floor, f floorf);
d!(ceil, ceil, f ceilf);
d!(trunc, trunc, f truncf);
d!(round, round, f roundf);
d!(asinh, asinh, f asinhf);
d!(acosh, acosh, f acoshf);
d!(atanh, atanh, f atanhf);

d2!(atan2, atan2, f atan2f);
d2!(fmod, fmod, f fmodf);
d2!(fmin, fmin, f fminf);
d2!(fmax, fmax, f fmaxf);
d2!(hypot, hypot, f hypotf);
d2!(pow, pow, f powf);
d2!(copysign, copysign, f copysignf);

#[unsafe(no_mangle)]
pub extern "C" fn fdim(x: c_double, y: c_double) -> c_double {
    libm::fdim(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn fma(x: c_double, y: c_double, z: c_double) -> c_double {
    libm::fma(x, y, z)
}

/// `modf` splits `x` into fractional and integral parts; the integral part
/// is stored at `*iptr`.
#[unsafe(no_mangle)]
pub extern "C" fn modf(x: c_double, iptr: *mut c_double) -> c_double {
    let t = libm::trunc(x);
    unsafe {
        *iptr = t;
    }
    x - t
}

/// `frexp` splits `x` into `m * 2^e`; the exponent is stored at `*eptr`.
#[unsafe(no_mangle)]
pub extern "C" fn frexp(x: c_double, eptr: *mut c_int) -> c_double {
    let (m, e) = libm::frexp(x);
    unsafe {
        *eptr = e as c_int;
    }
    m
}

/// `ldexp(x, e)` = `x * 2^e`.
#[unsafe(no_mangle)]
pub extern "C" fn ldexp(x: c_double, e: c_int) -> c_double {
    libm::ldexp(x, e as i32)
}

#[unsafe(no_mangle)]
pub extern "C" fn scalbn(x: c_double, n: c_int) -> c_double {
    libm::scalbn(x, n as i32)
}

/// Classify a double: 0 = FP_NAN, 1 = FP_INFINITE, 2 = FP_ZERO,
/// 3 = FP_SUBNORMAL, 4 = FP_NORMAL.
#[unsafe(no_mangle)]
pub extern "C" fn fpclassify(x: c_double) -> c_int {
    if x.is_nan() {
        0
    } else if x.is_infinite() {
        1
    } else if x == 0.0 {
        2
    } else if libm::fabs(x) < f64::MIN_POSITIVE {
        3
    } else {
        4
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn isnan(x: c_double) -> c_int {
    x.is_nan() as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isinf(x: c_double) -> c_int {
    if x.is_infinite() {
        if x < 0.0 { -1 } else { 1 }
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn isfinite(x: c_double) -> c_int {
    x.is_finite() as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isgreater(x: c_double, y: c_double) -> c_int {
    (x > y) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isless(x: c_double, y: c_double) -> c_int {
    (x < y) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isgreaterequal(x: c_double, y: c_double) -> c_int {
    (x >= y) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn islessequal(x: c_double, y: c_double) -> c_int {
    (x <= y) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn islessgreater(x: c_double, y: c_double) -> c_int {
    (x != y) as c_int
}

/// `nearbyint` — round to nearest integer as a double (ties away from zero,
/// matching libm's default mode).
#[unsafe(no_mangle)]
pub extern "C" fn nearbyint(x: c_double) -> c_double {
    libm::round(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn rint(x: c_double) -> c_double {
    libm::round(x)
}

/// `lrint` — round to nearest integer as a long.
#[unsafe(no_mangle)]
pub extern "C" fn lrint(x: c_double) -> c_long {
    libm::round(x) as c_long
}

/// `remainder(x, y)` = `x - n*y` with `n` the integer nearest `x/y`.
#[unsafe(no_mangle)]
pub extern "C" fn remainder(x: c_double, y: c_double) -> c_double {
    libm::remainder(x, y)
}

/// `pow` integer-exponent convenience is not a libm symbol; `powi` covers it.
#[unsafe(no_mangle)]
pub extern "C" fn powi(x: c_double, n: c_int) -> c_double {
    libm::pow(x, n as c_double)
}

/// `signbit(x)` — nonzero when the sign bit is set.
#[unsafe(no_mangle)]
pub extern "C" fn signbit(x: c_double) -> c_int {
    (x.to_bits() >> 63) as c_int
}

/// `nextafter(x, y)` — next representable double toward `y`.
#[unsafe(no_mangle)]
pub extern "C" fn nextafter(x: c_double, y: c_double) -> c_double {
    libm::nextafter(x, y)
}

#[unsafe(no_mangle)]
pub extern "C" fn nexttoward(x: c_double, _y: f64) -> c_double {
    x
}

d!(erf, erf, f erff);
d!(erfc, erfc, f erfcf);
d!(lgamma, lgamma, f lgammaf);
d!(tgamma, tgamma, f tgammaf);

#[unsafe(no_mangle)]
pub extern "C" fn j0(x: c_double) -> c_double { libm::j0(x) }
#[unsafe(no_mangle)]
pub extern "C" fn j1(x: c_double) -> c_double { libm::j1(x) }
#[unsafe(no_mangle)]
pub extern "C" fn y0(x: c_double) -> c_double { libm::y0(x) }
#[unsafe(no_mangle)]
pub extern "C" fn y1(x: c_double) -> c_double { libm::y1(x) }
#[unsafe(no_mangle)]
pub extern "C" fn yn(n: c_int, x: c_double) -> c_double { libm::yn(n, x) }

#[unsafe(no_mangle)]
pub extern "C" fn nan(_tag: *const c_char) -> c_double { f64::NAN }

#[unsafe(no_mangle)]
pub extern "C" fn logb(x: c_double) -> c_double { libm::ilogb(x) as c_double }

#[unsafe(no_mangle)]
pub extern "C" fn ilogb(x: c_double) -> c_int { libm::ilogb(x) }

#[unsafe(no_mangle)]
pub extern "C" fn remquo(x: c_double, y: c_double, quo: *mut c_int) -> c_double {
    let r = libm::remainder(x, y);
    if !quo.is_null() { unsafe { *quo = 0; } }
    r
}

#[unsafe(no_mangle)]
pub extern "C" fn scalbln(x: c_double, n: c_long) -> c_double { libm::scalbn(x, n as i32) }

#[unsafe(no_mangle)]
pub extern "C" fn llrint(x: c_double) -> c_longlong { libm::round(x) as c_longlong }

#[unsafe(no_mangle)]
pub extern "C" fn modff(x: c_float, iptr: *mut c_float) -> c_float {
    let t = libm::truncf(x);
    unsafe { *iptr = t; }
    x - t
}
