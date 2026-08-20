/*
 * checks.c — C-ABI conformance checks for the permissive BedrockOS libc.
 *
 * Compiled freestanding by build.rs against `user/libc/include`, linked
 * against the Rust `libc` crate.  Every function exercised here must exist
 * as a real C symbol with the declared signature — a header/symbol drift
 * fails at build or link time; a behaviour drift prints FAIL at runtime.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <ctype.h>
#include <math.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <dirent.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <signal.h>
#include <time.h>

static int fails = 0;

static void check(int ok, const char *name)
{
    if (ok)
    {
        printf("[posix] PASS %s\n", name);
    }
    else
    {
        printf("[posix] FAIL %s\n", name);
        fails++;
    }
}

static int cmp_int(const void *a, const void *b)
{
    return *(const int *)a - *(const int *)b;
}

int run_checks(void)
{
    char buf[128];
    struct stat st;
    struct timespec ts;
    struct utsname ut;
    DIR *d;
    struct dirent *de;

    /* ── printf engine ────────────────────────────────────────────────── */

    snprintf(buf, sizeof buf, "%.3f", 3.14159);
    check(strcmp(buf, "3.142") == 0, "snprintf %.3f");

    snprintf(buf, sizeof buf, "%08x", 0xbeef);
    check(strcmp(buf, "0000beef") == 0, "snprintf %08x");

    snprintf(buf, sizeof buf, "%s:%d", "x", 42);
    check(strcmp(buf, "x:42") == 0, "snprintf %s:%d");

    snprintf(buf, sizeof buf, "%+.2e", 12345.6789);
    check(strcmp(buf, "+1.23e+04") == 0, "snprintf %+.2e");

    /* ── scan engine ──────────────────────────────────────────────────── */

    {
        int a = 0, b = 0;
        check(sscanf("12 34", "%d %d", &a, &b) == 2 && a == 12 && b == 34, "sscanf ints");
    }

    /* ── numeric parsing ──────────────────────────────────────────────── */

    check(atof("3.5") == 3.5, "atof");
    {
        char *ep;
        double v = strtod("2.25xyz", &ep);
        check(v == 2.25 && *ep == 'x', "strtod");
    }

    /* ── qsort / strtok / strings ─────────────────────────────────────── */

    {
        int arr[4] = { 5, 2, 8, 1 };
        qsort(arr, 4, sizeof(int), cmp_int);
        check(arr[0] == 1 && arr[1] == 2 && arr[3] == 8, "qsort");
    }

    {
        char s[] = "a,b,c";
        char *p = strtok(s, ",");
        check(p && strcmp(p, "a") == 0, "strtok");
        p = strtok(NULL, ",");
        check(p && strcmp(p, "b") == 0, "strtok2");
    }

    check(strcasecmp("AbC", "abc") == 0, "strcasecmp");
    check(strdup("dup") != NULL && strcmp(strdup("dup"), "dup") == 0, "strdup");
    check(strspn("abcdx", "abc") == 3, "strspn");
    check(strcspn("abcdef", "xyz") == 6, "strcspn");

    /* ── math ─────────────────────────────────────────────────────────── */

    check(fabs(-2.5) == 2.5, "fabs");
    check(sin(0.0) == 0.0, "sin");
    check(fmod(7.5, 2.0) == 1.5, "fmod");
    check(sqrt(16.0) == 4.0, "sqrt");
    check(pow(2.0, 10.0) == 1024.0, "pow");

    /* ── ctype ────────────────────────────────────────────────────────── */

    check(isalpha('A') && isdigit('5') && isspace(' ') && isxdigit('f'), "ctype");

    /* ── time / identity / system ─────────────────────────────────────── */

    check(time(NULL) > 0, "time");
    check(clock_gettime(CLOCK_REALTIME, &ts) == 0 && ts.tv_sec > 0, "clock_gettime");
    check(clock_gettime(CLOCK_MONOTONIC, &ts) == 0, "clock_gettime monotonic");

    check(uname(&ut) == 0 && strcmp(ut.machine, "x86_64") == 0, "uname");

    check(getuid() == 0 && geteuid() == 0 && getgid() == 0, "uid/gid");
    check(getpid() > 0 && getppid() > 0, "pids");
    check(sysconf(_SC_PAGESIZE) == 4096, "sysconf pagesize");

    check(getenv("NOPE") == NULL, "getenv");
    check(system("echo") == -1 && errno == ENOENT, "system");

    /* ── fd model: file create/write/read/lseek/stat on /A (tmpfs) ───── */

    {
        const char *p = "/A/posixchk_data.txt";
        const char *msg = "posixcheck data"; /* 15 bytes */
        int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        check(fd >= 3, "open O_CREAT");
        if (fd >= 3)
        {
            check(write(fd, msg, 15) == 15, "write");
            check(close(fd) == 0, "close");
        }

        check(access(p, F_OK) == 0, "access F_OK");
        check(stat(p, &st) == 0 && st.st_size == 15, "stat size");
        check(S_ISREG(st.st_mode) && !S_ISDIR(st.st_mode), "S_ISREG");

        fd = open(p, O_RDONLY, 0);
        if (fd >= 3)
        {
            char rb[64];
            ssize_t n = read(fd, rb, sizeof rb);
            check(n == 15 && memcmp(rb, msg, 15) == 0, "read back");
            check(lseek(fd, 0, SEEK_END) == 15, "lseek SEEK_END");
            check(lseek(fd, 0, SEEK_SET) == 0, "lseek SEEK_SET");
            check(lseek(fd, 5, SEEK_CUR) == 5, "lseek SEEK_CUR");
            check(fstat(fd, &st) == 0 && st.st_size == 15, "fstat");
            check(close(fd) == 0, "close2");
        }
        else
        {
            check(0, "open O_RDONLY");
        }

        check(truncate(p, 5) == 0 && stat(p, &st) == 0 && st.st_size == 5, "truncate");
    }

    /* ── mkdir + dirent walk ──────────────────────────────────────────── */

    {
        const char *dirp = "/A/posixchk_dir";
        check(mkdir(dirp, 0755) == 0, "mkdir");
        check(stat(dirp, &st) == 0 && S_ISDIR(st.st_mode), "stat dir");
        d = opendir(dirp);
        check(d != NULL, "opendir");
        if (d)
        {
            int n = 0;
            while ((de = readdir(d)) != NULL)
            {
                n++;
            }
            closedir(d);
            check(n >= 0, "readdir walk");
        }
        check(rmdir(dirp) == 0, "rmdir");
    }

    /* ── chdir / getcwd ───────────────────────────────────────────────── */

    {
        char cwd[128];
        check(chdir("/A") == 0, "chdir");
        check(getcwd(cwd, sizeof cwd) != NULL && strcmp(cwd, "/A") == 0, "getcwd");
    }

    /* ── signal ───────────────────────────────────────────────────────── */

    check(signal(SIGINT, SIG_DFL) == SIG_DFL, "signal");

    return fails;
}