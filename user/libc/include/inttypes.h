/*
 * BedrockOS permissive libc — <inttypes.h>
 */
#ifndef BEDROCK_LIBC_INTTYPES_H
#define BEDROCK_LIBC_INTTYPES_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int64_t  intmax_t;
typedef uint64_t uintmax_t;

#define PRId64 "ld"
#define PRIu64 "lu"
#define PRIx64 "lx"

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_INTTYPES_H */