/*
 * BedrockOS permissive libc — <stddef.h>
 * Freestanding; provides ptrdiff_t/size_t/wchar_t/NULL/offsetof.
 */
#ifndef BEDROCK_LIBC_STDDEF_H
#define BEDROCK_LIBC_STDDEF_H

typedef long ptrdiff_t;
typedef unsigned long size_t;
typedef unsigned int wchar_t;

#ifndef NULL
#ifdef __cplusplus
#define NULL 0L
#else
#define NULL ((void*)0)
#endif
#endif

#define offsetof(type, member) __builtin_offsetof(type, member)

#ifndef max_align_t
typedef struct { long long __ll; long double __ld; } max_align_t;
#endif

#endif /* BEDROCK_LIBC_STDDEF_H */
