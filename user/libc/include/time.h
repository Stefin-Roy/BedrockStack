/*
 * BedrockOS permissive libc — <time.h>
 *
 * Implemented in Rust (time.rs).  Realtime from /kernel/timer:epoch_secs,
 * monotonic from /kernel/timer.
 */
#ifndef BEDROCK_LIBC_TIME_H
#define BEDROCK_LIBC_TIME_H

#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

#define CLOCKS_PER_SEC 1000000L

#define CLOCK_REALTIME  0
#define CLOCK_MONOTONIC 1

struct timespec {
    time_t tv_sec;
    long   tv_nsec;
};

struct timeval {
    time_t      tv_sec;
    long        tv_usec;
};

time_t time(time_t *tloc);
int    gettimeofday(struct timeval *tv, void *tz);
int    clock_gettime(int clockid, struct timespec *ts);
int    nanosleep(const struct timespec *req, struct timespec *rem);
long   clock(void);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_TIME_H */