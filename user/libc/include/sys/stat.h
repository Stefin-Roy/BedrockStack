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
#define S_IFLNK  0120000
#define S_IFIFO  0010000
#define S_IFCHR  0020000
#define S_IFBLK  0060000
#define S_IFSOCK 0140000

#define S_ISREG(m) (((m) & S_IFMT) == S_IFREG)
#define S_ISDIR(m) (((m) & S_IFMT) == S_IFDIR)
#define S_ISLNK(m) (((m) & S_IFMT) == S_IFLNK)
#define S_ISFIFO(m) (((m) & S_IFMT) == S_IFIFO)
#define S_ISCHR(m) (((m) & S_IFMT) == S_IFCHR)
#define S_ISBLK(m) (((m) & S_IFMT) == S_IFBLK)
#define S_ISSOCK(m) (((m) & S_IFMT) == S_IFSOCK)

#define S_IRWXU 00700
#define S_IRUSR 00400
#define S_IWUSR 00200
#define S_IXUSR 00100
#define S_IRWXG 00070
#define S_IRGRP 00040
#define S_IWGRP 00020
#define S_IXGRP 00010
#define S_IRWXO 00007
#define S_IROTH 00004
#define S_IWOTH 00002
#define S_IXOTH 00001

int stat(const char *path, struct stat *buf);
int lstat(const char *path, struct stat *buf);
int fstat(int fd, struct stat *buf);

int mkdir(const char *path, mode_t mode);
int rmdir(const char *path);
int truncate(const char *path, long long length);
int ftruncate(int fd, long length);
int chmod(const char *path, mode_t mode);
int fchmod(int fd, mode_t mode);
int fchmodat(int dirfd, const char *path, mode_t mode, int flags);
mode_t umask(mode_t mask);
int mkfifo(const char *path, mode_t mode);
int mknod(const char *path, mode_t mode, dev_t dev);
int stat(const char *path, struct stat *buf);
int fstatat(int dirfd, const char *path, struct stat *buf, int flags);
int lstat(const char *path, struct stat *buf);
int futimens(int fd, const struct timespec *times);
int utimensat(int dirfd, const char *path, const struct timespec *times, int flags);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_SYS_STAT_H */