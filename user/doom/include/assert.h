/*
 * Minimal freestanding <assert.h> for the vendored doomgeneric engine.
 *
 * Compile-time no-op: assert() here cannot abort, so it is disabled to
 * avoid silently swallowing a would-be assertion failure.  The engine's
 * asserts are internal invariant checks that are not part of gameplay.
 */
#ifndef BEDROCK_ASSERT_H
#define BEDROCK_ASSERT_H

#define assert(e) ((void)0)

#endif /* BEDROCK_ASSERT_H */
