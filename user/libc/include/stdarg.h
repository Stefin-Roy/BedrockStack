/*
 * BedrockOS permissive libc — <stdarg.h>
 * Wrapper over compiler builtin. Provided for freestanding -fno-builtin builds
 * where <stdarg.h> must still be findable under user/libc/include.
 * Defer to the compiler's builtin header.
 */
#ifndef BEDROCK_LIBC_STDARG_H
#define BEDROCK_LIBC_STDARG_H

#ifdef __has_include_next
#if __has_include_next(<stdarg.h>)
#include_next <stdarg.h>
#else
typedef __builtin_va_list va_list;
#define va_start(ap, last) __builtin_va_start(ap, last)
#define va_arg(ap, type) __builtin_va_arg(ap, type)
#define va_end(ap) __builtin_va_end(ap)
#define va_copy(dest, src) __builtin_va_copy(dest, src)
#endif
#else
typedef __builtin_va_list va_list;
#define va_start(ap, last) __builtin_va_start(ap, last)
#define va_arg(ap, type) __builtin_va_arg(ap, type)
#define va_end(ap) __builtin_va_end(ap)
#define va_copy(dest, src) __builtin_va_copy(dest, src)
#endif

#endif /* BEDROCK_LIBC_STDARG_H */
