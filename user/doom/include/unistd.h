/*
 * Minimal freestanding <unistd.h> for the vendored doomgeneric engine.
 * Included by a handful of engine files; nothing of substance is used.
 */
#ifndef BEDROCK_UNISTD_H
#define BEDROCK_UNISTD_H

#ifdef __cplusplus
extern "C" {
#endif

int close(int fd);
int read(int fd, void *buf, unsigned long count);
int write(int fd, const void *buf, unsigned long count);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_UNISTD_H */
