/*
 * BedrockOS permissive libc — <sys/time.h>
 * gettimeofday / settimeofday / timers via /kernel/timer.
 */
#ifndef BEDROCK_LIBC_SYS_TIME_H
#define BEDROCK_LIBC_SYS_TIME_H

#include <sys/types.h>
#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

struct timeval {
    time_t tv_sec;
    long tv_usec;
};

struct timezone {
    int tz_minuteswest;
    int tz_dsttime;
};

int gettimeofday(struct timeval *tv, struct timezone *tz);
int settimeofday(const struct timeval *tv, const struct timezone *tz);
int utimes(const char *path, const struct timeval times[2]);
int futimes(int fd, const struct timeval times[2]);
int select(int nfds, void *readfds, void *writefds, void *exceptfds, struct timeval *timeout);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_SYS_TIME_H */
