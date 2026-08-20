/*
 * BedrockOS permissive libc — <sys/types.h>
 */
#ifndef BEDROCK_LIBC_SYS_TYPES_H
#define BEDROCK_LIBC_SYS_TYPES_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef long          ssize_t;
typedef long          off_t;
typedef unsigned int  mode_t;
typedef unsigned int  uid_t;
typedef unsigned int  gid_t;
typedef unsigned int  pid_t;
typedef unsigned int  dev_t;
typedef unsigned int  ino_t;
typedef unsigned int  nlink_t;
typedef long          time_t;

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_SYS_TYPES_H */