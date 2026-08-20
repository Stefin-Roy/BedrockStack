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

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_STRINGS_H */