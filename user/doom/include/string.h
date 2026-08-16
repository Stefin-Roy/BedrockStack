/*
 * Minimal freestanding <string.h> for the vendored doomgeneric engine.
 *
 * strlen/strnlen/strcmp/strncmp/strcpy/strncpy/strcat/strncat/strchr/
 * strrchr/strstr/memchr/atoi/strtol/strdup are implemented by `user/libc`.
 * memset/memcpy/memmove/memcmp come from Rust compiler-builtins.
 * strcasecmp/strncasecmp are provided by shim.c (declared in <strings.h>).
 */
#ifndef BEDROCK_STRING_H
#define BEDROCK_STRING_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

size_t strlen(const char *s);
size_t strnlen(const char *s, size_t maxlen);
int    strcmp(const char *a, const char *b);
int    strncmp(const char *a, const char *b, size_t n);
char  *strcpy(char *dst, const char *src);
char  *strncpy(char *dst, const char *src, size_t n);
char  *strcat(char *dst, const char *src);
char  *strncat(char *dst, const char *src, size_t n);
char  *strchr(const char *s, int c);
char  *strrchr(const char *s, int c);
char  *strstr(const char *haystack, const char *needle);
char  *strdup(const char *s);

void  *memchr(const void *s, int c, size_t n);
int    memcmp(const void *a, const void *b, size_t n);
void  *memcpy(void *dst, const void *src, size_t n);
void  *memmove(void *dst, const void *src, size_t n);
void  *memset(void *dst, int c, size_t n);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_STRING_H */
