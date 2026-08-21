/*
 * BedrockOS permissive libc — <unistd.h>
 *
 * Implemented in Rust (unistd.rs / fd.rs / mem.rs / process.rs / stdio.rs).
 */
#ifndef BEDROCK_LIBC_UNISTD_H
#define BEDROCK_LIBC_UNISTD_H

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <sys/stat.h>

#ifdef __cplusplus
extern "C" {
#endif

#define STDIN_FILENO  0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

/* access() mode bits */
#define F_OK 0
#define X_OK 1
#define W_OK 2
#define R_OK 4

/* sysconf() names */
#define _SC_PAGESIZE        30
#define _SC_PAGE_SIZE       30
#define _SC_NPROCESSORS_CONF 83
#define _SC_NPROCESSORS_ONLN 84
#define _SC_PHYS_PAGES      85

ssize_t read(int fd, void *buf, size_t count);
ssize_t write(int fd, const void *buf, size_t count);
int     close(int fd);
off_t   lseek(int fd, off_t offset, int whence);
int     ftruncate(int fd, off_t length);
int     fstat(int fd, struct stat *buf);
int     dup(int fd);
int     dup2(int oldfd, int newfd);

int     chdir(const char *path);
char   *getcwd(char *buf, size_t size);
int     access(const char *path, int mode);
int     isatty(int fd);
int     unlink(const char *path);

uid_t   getuid(void);
uid_t   geteuid(void);
gid_t   getgid(void);
gid_t   getegid(void);
int     setuid(uid_t uid);
int     setgid(gid_t gid);
int     getgroups(int size, int *list);

long    sysconf(int name);
long    pathconf(const char *path, int name);
long    fpathconf(int fd, int name);
long    confstr(int name, char *buf, size_t len);

void   *sbrk(intptr_t increment);
int     brk(void *addr);

pid_t   getpid(void);
pid_t   getppid(void);
pid_t   fork(void);
int     execve(const char *path, char *const argv[], char *const envp[]);
int     execv(const char *path, char *const argv[]);
int     execvp(const char *file, char *const argv[]);
int     execl(const char *path, const char *arg, ...);
int     execlp(const char *file, const char *arg, ...);
void    _exit(int status) __attribute__((noreturn));
int     pause(void);

unsigned int sleep(unsigned int seconds);
int          usleep(unsigned int useconds);
int     gethostname(char *name, size_t len);
int     getopt(int argc, char *const argv[], const char *optstring);
extern char *optarg;
extern int optind, opterr, optopt;
int     pipe(int fds[2]);
int     link(const char *oldpath, const char *newpath);
int     symlink(const char *target, const char *linkpath);
ssize_t readlink(const char *path, char *buf, size_t bufsiz);
int     chown(const char *path, uid_t owner, gid_t group);
int     fchown(int fd, uid_t owner, gid_t group);
int     unlink(const char *path);
int     rmdir(const char *path);

#define _PC_NAME_MAX 4
#define _PC_PATH_MAX 5
#define _PC_PIPE_BUF 6

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_UNISTD_H */