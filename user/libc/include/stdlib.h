/*
 * BedrockOS permissive libc — <stdlib.h>
 *
 * Implemented in Rust (stdlib.rs / mem.rs / process.rs / format.rs).
 * `qsort`/`bsearch` callers must use the standard 4-arg comparators.
 */
#ifndef BEDROCK_LIBC_STDLIB_H
#define BEDROCK_LIBC_STDLIB_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX 2147483647

void  exit(int code) __attribute__((noreturn));
void  _Exit(int code) __attribute__((noreturn));
void  abort(void) __attribute__((noreturn));

typedef struct { int quot, rem; } div_t;
typedef struct { long quot, rem; } ldiv_t;

int   abs(int v);
long  labs(long v);
long long llabs(long long v);
div_t div(int numer, int denom);
ldiv_t ldiv(long numer, long denom);

int   rand(void);
void  srand(unsigned int seed);

void *malloc(size_t size);
void *calloc(size_t nmemb, size_t size);
void *realloc(void *ptr, size_t size);
void  free(void *ptr);

int   atoi(const char *s);
long  atol(const char *s);
long long atoll(const char *s);
double atof(const char *s);

long   strtol(const char *s, char **endptr, int base);
long long strtoll(const char *s, char **endptr, int base);
unsigned long   strtoul(const char *s, char **endptr, int base);
unsigned long long strtoull(const char *s, char **endptr, int base);
float  strtof(const char *s, char **endptr);
double strtod(const char *s, char **endptr);

void  qsort(void *base, size_t nmemb, size_t size,
            int (*cmp)(const void *, const void *));
void *bsearch(const void *key, const void *base, size_t nmemb, size_t size,
              int (*cmp)(const void *, const void *));

char *getenv(const char *name);
int   system(const char *cmd);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_STDLIB_H */