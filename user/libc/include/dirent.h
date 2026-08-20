/*
 * BedrockOS permissive libc — <dirent.h>
 *
 * Implemented in Rust (dirent.rs) over the VFS directory-listing wire.
 */
#ifndef BEDROCK_LIBC_DIRENT_H
#define BEDROCK_LIBC_DIRENT_H

#include <stddef.h>
#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DT_UNKNOWN 0
#define DT_DIR     4
#define DT_REG     8

typedef struct DIR DIR;

struct dirent {
    unsigned long long d_ino;   /* 0 */
    unsigned char      d_type;  /* 8 */
    char               d_name[256];
};

DIR  *opendir(const char *path);
struct dirent *readdir(DIR *dir);
int   closedir(DIR *dir);
void  rewinddir(DIR *dir);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_DIRENT_H */