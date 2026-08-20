/*
 * BedrockOS permissive libc — <sys/stat.h>
 *
 * Implemented in Rust (vfs.rs / fd.rs).  `stat()`/`lstat()`/`fstat()` fill
 * this 32-byte layout: st_ino, st_size, st_mode (S_IFREG/S_IFDIR bits),
 * st_mtime.
 */
#ifndef BEDROCK_LIBC_SYS_STAT_H
#define BEDROCK_LIBC_SYS_STAT_H

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

struct stat {
    unsigned long long st_ino;   /* 0  */
    unsigned long long st_size;  /* 8  */
    unsigned int       st_mode;  /* 16 */
    unsigned long long st_mtime; /* 24 */
};

#define S_IFMT   0170000
#define S_IFREG  0100000
#define S_IFDIR  0040000

#define S_ISREG(m) (((m) & S_IFMT) == S_IFREG)
#define S_ISDIR(m) (((m) & S_IFMT) == S_IFDIR)

int stat(const char *path, struct stat *buf);
int lstat(const char *path, struct stat *buf);
int fstat(int fd, struct stat *buf);

int mkdir(const char *path, mode_t mode);
int rmdir(const char *path);
int truncate(const char *path, long long length);
int ftruncate(int fd, long length);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_SYS_STAT_H */