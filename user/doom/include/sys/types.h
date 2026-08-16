/*
 * Minimal freestanding <sys/types.h> for the vendored doomgeneric engine.
 * Provides the size_t/ssize_t the engine includes it for.
 */
#ifndef BEDROCK_SYS_TYPES_H
#define BEDROCK_SYS_TYPES_H

#include <stddef.h>

typedef long ssize_t;

#endif /* BEDROCK_SYS_TYPES_H */
