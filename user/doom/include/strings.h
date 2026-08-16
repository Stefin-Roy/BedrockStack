/*
 * Minimal freestanding <strings.h> for the vendored doomgeneric engine.
 * (doomtype.h includes this unconditionally on non-Windows targets.)
 * strcasecmp/strncasecmp are implemented in shim.c.
 */
#ifndef BEDROCK_STRINGS_H
#define BEDROCK_STRINGS_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

int strcasecmp(const char *a, const char *b);
int strncasecmp(const char *a, const char *b, size_t n);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_STRINGS_H */
