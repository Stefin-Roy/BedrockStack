/*
 * BedrockOS permissive libc — <assert.h>
 */
#ifndef BEDROCK_LIBC_ASSERT_H
#define BEDROCK_LIBC_ASSERT_H

#include <stdio.h>
#include <stdlib.h>

#ifdef NDEBUG
#define assert(e) ((void)0)
#else
#define assert(e) \
    ((e) ? (void)0 \
         : (fprintf(stderr, "Assertion failed: %s at %s:%d\n", \
                    #e, __FILE__, __LINE__), \
            abort()))
#endif

#endif /* BEDROCK_LIBC_ASSERT_H */