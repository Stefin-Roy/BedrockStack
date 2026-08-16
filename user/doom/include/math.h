/*
 * Minimal freestanding <math.h> for the vendored doomgeneric engine.
 *
 * Only fabs() is referenced by live engine code (v_video.c).  sin()/cos()
 * are declared defensively (r_main.c's use sits inside `#if 0`); they are
 * implemented in shim.c with small self-contained approximations.
 */
#ifndef BEDROCK_MATH_H
#define BEDROCK_MATH_H

#ifdef __cplusplus
extern "C" {
#endif

double fabs(double v);
double sin(double v);
double cos(double v);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_MATH_H */
