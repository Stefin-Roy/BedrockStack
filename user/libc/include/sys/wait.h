/*
 * BedrockOS permissive libc — <sys/wait.h>
 *
 * Implemented in Rust (process.rs).  The kernel reports only exit codes, so
 * every reaped child counts as normally exited with status = code & 0xFF.
 */
#ifndef BEDROCK_LIBC_SYS_WAIT_H
#define BEDROCK_LIBC_SYS_WAIT_H

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

#define WNOHANG    1
#define WUNTRACED  2

#define WIFEXITED(s)    ((s) != -1)
#define WEXITSTATUS(s)  ((s) & 0xff)
#define WIFSIGNALED(s)  (0)
#define WTERMSIG(s)     ((s) & 0x7f)

pid_t wait(int *status);
pid_t waitpid(pid_t pid, int *status, int options);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_SYS_WAIT_H */