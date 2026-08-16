/*
 * Minimal freestanding <sys/stat.h> for the vendored doomgeneric engine.
 * m_misc.c M_EnsureDirectory uses 2-arg mkdir(path, 0755); shim.c stubs
 * it as always-successful.  S_IFREG used by statdump.c.
 */
#ifndef BEDROCK_SYS_STAT_H
#define BEDROCK_SYS_STAT_H

#ifdef __cplusplus
extern "C" {
#endif

struct stat
{
    unsigned long long st_dev;
    unsigned long long st_ino;
    unsigned long long st_nlink;
    int                st_mode;
    unsigned long long st_uid;
    unsigned long long st_gid;
    unsigned long long st_rdev;
    long long          st_size;
};

#define S_IFMT   0170000
#define S_IFREG  0100000
#define S_IFDIR  0040000

#define S_ISREG(m) (((m) & S_IFMT) == S_IFREG)
#define S_ISDIR(m) (((m) & S_IFMT) == S_IFDIR)

int mkdir(const char *path, int mode);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_SYS_STAT_H */
