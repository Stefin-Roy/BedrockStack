/*
 * BedrockOS permissive libc — <limits.h>
 * C locale, no dynamic limits. Values match gcc/x86_64 LP64.
 */
#ifndef BEDROCK_LIBC_LIMITS_H
#define BEDROCK_LIBC_LIMITS_H

#define CHAR_BIT 8
#define MB_LEN_MAX 1

#define SCHAR_MIN (-128)
#define SCHAR_MAX 127
#define UCHAR_MAX 255

#ifdef __CHAR_UNSIGNED__
#define CHAR_MIN 0
#define CHAR_MAX UCHAR_MAX
#else
#define CHAR_MIN SCHAR_MIN
#define CHAR_MAX SCHAR_MAX
#endif

#define SHRT_MIN (-32768)
#define SHRT_MAX 32767
#define USHRT_MAX 65535

#define INT_MIN (-2147483647 - 1)
#define INT_MAX 2147483647
#define UINT_MAX 4294967295U

#define LONG_MIN (-9223372036854775807L - 1)
#define LONG_MAX 9223372036854775807L
#define ULONG_MAX 18446744073709551615UL

#define LLONG_MIN (-9223372036854775807LL - 1)
#define LLONG_MAX 9223372036854775807LL
#define ULLONG_MAX 18446744073709551615ULL

/* POSIX / path limits (Bedrock VFS) */
#define PATH_MAX 4096
#define NAME_MAX 255
#define PIPE_BUF 512
#define ARG_MAX 4096

/* POSIX minimal guarantees */
#define _POSIX_NAME_MAX 14
#define _POSIX_PATH_MAX 256
#define OPEN_MAX 32
#define _POSIX_OPEN_MAX 20

#endif /* BEDROCK_LIBC_LIMITS_H */
