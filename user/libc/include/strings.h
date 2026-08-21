/*
 * BedrockOS permissive libc — <strings.h>
 *
 * BSD string functions, implemented in Rust (string.rs).
 */
#ifndef BEDROCK_LIBC_STRINGS_H
#define BEDROCK_LIBC_STRINGS_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

int strcasecmp(const char *a, const char *b);
int strncasecmp(const char *a, const char *b, size_t n);
int ffs(int i);
int ffsl(long i);
int ffsll(long long i);
int bcmp(const void *a, const void *b, size_t n);
void bcopy(const void *src, void *dst, size_t n);
void bzero(void *s, size_t n);
char *index(const char *s, int c);
char *rindex(const char *s, int c);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_STRINGS_H */