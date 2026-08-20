/*
 * BedrockOS permissive libc — <fcntl.h>
 *
 * Implemented in Rust (fd.rs).  Flag values are Linux ABI numbers.
 */
#ifndef BEDROCK_LIBC_FCNTL_H
#define BEDROCK_LIBC_FCNTL_H

#ifdef __cplusplus
extern "C" {
#endif

#define O_RDONLY   0000000
#define O_WRONLY   0000001
#define O_RDWR     0000002
#define O_ACCMODE  0000003
#define O_CREAT    0000100
#define O_EXCL     0000200
#define O_TRUNC    0001000
#define O_APPEND   0002000
#define O_NONBLOCK 0004000

#define F_GETFL 3
#define F_SETFL 4

int open(const char *path, int flags, ...);
int creat(const char *path, unsigned int mode);
int fcntl(int fd, int cmd, ...);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_FCNTL_H */