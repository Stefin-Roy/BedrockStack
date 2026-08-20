/*
 * BedrockOS permissive libc — <string.h>
 *
 * Implemented in Rust (string.rs).  memset/memcpy/memmove/memcmp come from
 * Rust compiler-builtins (exported by the kernel toolchain).
 */
#ifndef BEDROCK_LIBC_STRING_H
#define BEDROCK_LIBC_STRING_H

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
size_t strspn(const char *s, const char *accept);
size_t strcspn(const char *s, const char *reject);
char  *strpbrk(const char *s, const char *accept);
char  *strtok(char *s, const char *delim);
char  *strtok_r(char *s, const char *delim, char **saveptr);
int    strcoll(const char *a, const char *b);
size_t strxfrm(char *dst, const char *src, size_t n);

void  *memchr(const void *s, int c, size_t n);
int    memcmp(const void *a, const void *b, size_t n);
void  *memcpy(void *dst, const void *src, size_t n);
void  *memmove(void *dst, const void *src, size_t n);
void  *memset(void *dst, int c, size_t n);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_STRING_H */