/*
 * BedrockOS permissive libc — <sys/utsname.h>
 *
 * Implemented in Rust (unistd.rs); release comes from /sys/version.
 */
#ifndef BEDROCK_LIBC_SYS_UTSNAME_H
#define BEDROCK_LIBC_SYS_UTSNAME_H

#ifdef __cplusplus
extern "C" {
#endif

#define UTSNAME_LEN 65

struct utsname {
    char sysname[UTSNAME_LEN];
    char nodename[UTSNAME_LEN];
    char release[UTSNAME_LEN];
    char version[UTSNAME_LEN];
    char machine[UTSNAME_LEN];
};

int uname(struct utsname *buf);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_SYS_UTSNAME_H */