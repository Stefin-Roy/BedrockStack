/*
 * BedrockOS permissive libc — <math.h>
 *
 * Implemented in Rust (math.rs) backed by the permissive no_std `libm`
 * crate.  Double and float entry points are exported.
 */
#ifndef BEDROCK_LIBC_MATH_H
#define BEDROCK_LIBC_MATH_H

#ifdef __cplusplus
extern "C" {
#endif

#define HUGE_VAL (1.0 / 0.0)

#define FP_NAN      0
#define FP_INFINITE 1
#define FP_ZERO     2
#define FP_SUBNORMAL 3
#define FP_NORMAL   4

double fabs(double x);
double sin(double x);
double cos(double x);
double tan(double x);
double asin(double x);
double acos(double x);
double atan(double x);
double sinh(double x);
double cosh(double x);
double tanh(double x);
double sqrt(double x);
double cbrt(double x);
double exp(double x);
double exp2(double x);
double expm1(double x);
double log(double x);
double log10(double x);
double log2(double x);
double log1p(double x);
double floor(double x);
double ceil(double x);
double trunc(double x);
double round(double x);
double asinh(double x);
double acosh(double x);
double atanh(double x);

double atan2(double y, double x);
double fmod(double x, double y);
double fmin(double x, double y);
double fmax(double x, double y);
double hypot(double x, double y);
double pow(double x, double y);
double copysign(double x, double y);
double fdim(double x, double y);
double fma(double x, double y, double z);

float  fabsf(float x);
float  sinf(float x);
float  cosf(float x);
float  tanf(float x);
float  asinf(float x);
float  acosf(float x);
float  atanf(float x);
float  sinhf(float x);
float  coshf(float x);
float  tanhf(float x);
float  sqrtf(float x);
float  cbrtf(float x);
float  expf(float x);
float  exp2f(float x);
float  expm1f(float x);
float  logf(float x);
float  log10f(float x);
float  log2f(float x);
float  log1pf(float x);
float  floorf(float x);
float  ceilf(float x);
float  truncf(float x);
float  roundf(float x);
float  asinhf(float x);
float  acoshf(float x);
float  atanhf(float x);
float  atan2f(float y, float x);
float  fmodf(float x, float y);
float  fminf(float x, float y);
float  fmaxf(float x, float y);
float  hypotf(float x, float y);
float  powf(float x, float y);
float  copysignf(float x, float y);

double modf(double x, double *iptr);
float modff(float x, float *iptr);
double frexp(double x, int *eptr);
double ldexp(double x, int e);
double scalbn(double x, int n);
double scalbln(double x, long n);
double nearbyint(double x);
double rint(double x);
long   lrint(double x);
long long llrint(double x);
double remainder(double x, double y);
double remquo(double x, double y, int *quo);
double nextafter(double x, double y);
double nexttoward(double x, long double y);
double erf(double x);
float erff(float x);
double erfc(double x);
float erfcf(float x);
double lgamma(double x);
float lgammaf(float x);
double tgamma(double x);
float tgammaf(float x);
double j0(double x);
double j1(double x);
double y0(double x);
double y1(double x);
double yn(int n, double x);
double nan(const char *tagp);
double logb(double x);
int ilogb(double x);

int fpclassify(double x);
int isnan(double x);
int isinf(double x);
int isfinite(double x);
int isgreater(double x, double y);
int isless(double x, double y);
int isgreaterequal(double x, double y);
int islessequal(double x, double y);
int islessgreater(double x, double y);
int signbit(double x);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_MATH_H */