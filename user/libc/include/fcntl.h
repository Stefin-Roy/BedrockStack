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
#define O_CLOEXEC  00200000
#define O_DIRECTORY 00200000
#define O_NOFOLLOW 00400000
#define O_SYNC     00010000
#define O_DSYNC    00010000

#define F_DUPFD  0
#define F_GETFD  1
#define F_SETFD  2
#define F_GETFL  3
#define F_SETFL  4
#define F_GETLK  5
#define F_SETLK  6
#define F_SETLKW 7

#define FD_CLOEXEC 1

int open(const char *path, int flags, ...);
int openat(int dirfd, const char *path, int flags, ...);
int creat(const char *path, unsigned int mode);
int fcntl(int fd, int cmd, ...);
int posix_fadvise(int fd, long offset, long len, int advice);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_FCNTL_H */