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
int    settimeofday(const struct timeval *tv, const struct timezone *tz);
int    clock_gettime(int clockid, struct timespec *ts);
int    clock_getres(int clockid, struct timespec *res);
int    nanosleep(const struct timespec *req, struct timespec *rem);
long   clock(void);
double difftime(time_t t1, time_t t0);
time_t mktime(struct tm *tm);
struct tm *gmtime(const time_t *t);
struct tm *gmtime_r(const time_t *t, struct tm *result);
struct tm *localtime(const time_t *t);
struct tm *localtime_r(const time_t *t, struct tm *result);
char *asctime(const struct tm *tm);
char *asctime_r(const struct tm *tm, char *buf);
char *ctime(const time_t *t);
char *ctime_r(const time_t *t, char *buf);
size_t strftime(char *s, size_t max, const char *fmt, const struct tm *tm);
char *strptime(const char *s, const char *fmt, struct tm *tm);
void tzset(void);
struct tm {
    int tm_sec;
    int tm_min;
    int tm_hour;
    int tm_mday;
    int tm_mon;
    int tm_year;
    int tm_wday;
    int tm_yday;
    int tm_isdst;
    long tm_gmtoff;
    const char *tm_zone;
};

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_TIME_H */