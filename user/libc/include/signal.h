/*
 * BedrockOS permissive libc — <signal.h>
 *
 * Implemented in Rust (signal.rs / process.rs).  The kernel delivers no
 * signals; handlers are accepted but never invoked.  raise() maps to kill.
 */
#ifndef BEDROCK_LIBC_SIGNAL_H
#define BEDROCK_LIBC_SIGNAL_H

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SIGHUP    1
#define SIGINT    2
#define SIGQUIT   3
#define SIGILL    4
#define SIGTRAP   5
#define SIGABRT   6
#define SIGBUS    7
#define SIGFPE    8
#define SIGKILL   9
#define SIGUSR1  10
#define SIGSEGV  11
#define SIGUSR2  12
#define SIGPIPE  13
#define SIGALRM  14
#define SIGTERM  15
#define SIGCHLD  17
#define SIGCONT  18
#define SIGSTOP  19
#define SIGTSTP  20
#define SIGTTIN  21
#define SIGTTOU  22
#define SIGURG   23
#define SIGXCPU  24
#define SIGXFSZ  25
#define SIGVTALRM 26
#define SIGPROF  27
#define SIGWINCH 28
#define SIGIO    29
#define SIGSYS   31

typedef void (*sighandler_t)(int);

#define SIG_DFL ((void (*)(int))0)
#define SIG_IGN ((void (*)(int))1)
#define SIG_ERR ((void (*)(int))-1)

typedef struct { unsigned long __bits; } sigset_t;

sighandler_t signal(int sig, sighandler_t handler);
int raise(int sig);
int kill(pid_t pid, int sig);
unsigned int alarm(unsigned int seconds);

int sigemptyset(sigset_t *set);
int sigfillset(sigset_t *set);
int sigaction(int sig, const void *act, void *oldact);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_SIGNAL_H */