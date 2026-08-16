/*
 * Minimal freestanding <stdlib.h> for the vendored doomgeneric engine.
 *
 * malloc/calloc/realloc/free come from `user/libc`; the rest (exit, abs,
 * labs, atof, getenv, system, qsort, ...) is implemented in shim.c.
 */
#ifndef BEDROCK_STDLIB_H
#define BEDROCK_STDLIB_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

void *malloc(size_t size);
void *calloc(size_t nmemb, size_t size);
void *realloc(void *ptr, size_t size);
void  free(void *ptr);

void  exit(int code) __attribute__((noreturn));
void  abort(void) __attribute__((noreturn));

int   atoi(const char *s);
long  atol(const char *s);
double atof(const char *s);
long  strtol(const char *s, char **endptr, int base);
double strtod(const char *s, char **endptr);

int   abs(int v);
long  labs(long v);

char  *getenv(const char *name);
int   system(const char *cmd);

void  qsort(void *base, size_t nmemb, size_t size,
            int (*cmp)(const void *, const void *));

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_STDLIB_H */
