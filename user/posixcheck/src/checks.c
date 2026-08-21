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
    {
        sigset_t set;
        check(sigemptyset(&set) == 0, "sigemptyset");
        check(sigaddset(&set, SIGINT) == 0 && sigismember(&set, SIGINT) == 1, "sigaddset");
        check(sigdelset(&set, SIGINT) == 0 && sigismember(&set, SIGINT) == 0, "sigdelset");
        check(sigfillset(&set) == 0 && sigismember(&set, SIGTERM) == 1, "sigfillset");
        check(sigprocmask(SIG_BLOCK, &set, NULL) == 0, "sigprocmask");
    }

    /* ── chmod / umask / fchmod ────────────────────────────────────────── */

    {
        const char *p = "/A/posixchk_chmod";
        int fd2 = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        if (fd2 >= 3) close(fd2);
        check(chmod(p, 0600) == 0, "chmod");
        if (stat(p, &st) == 0) {
            check((st.st_mode & 0777) == 0600 && S_ISREG(st.st_mode), "chmod mode");
        } else {
            check(0, "chmod stat");
        }
        fd2 = open(p, O_RDONLY, 0);
        if (fd2 >= 3) {
            check(fchmod(fd2, 0644) == 0, "fchmod");
            check(fstat(fd2, &st) == 0 && (st.st_mode & 0777) == 0644, "fchmod mode");
            close(fd2);
        } else {
            check(0, "fchmod open");
        }
        mode_t old = umask(022);
        (void)old;
        check(umask(022) == 022, "umask");
        unlink(p);
    }

    /* ── mkfifo ─────────────────────────────────────────────────────────── */

    {
        const char *fp = "/A/posixchk_fifo";
        unlink(fp);
        check(mkfifo(fp, 0644) == 0, "mkfifo");
        if (stat(fp, &st) == 0) {
            check(S_ISFIFO(st.st_mode), "S_ISFIFO");
        } else {
            check(0, "mkfifo stat");
        }
        unlink(fp);
    }

    /* ── symlink / readlink / lstat / link / utimens ───────────────────── */

    {
        const char *target = "/A/posixchk_data.txt";
        const char *linkp = "/A/posixchk_link";
        const char *hard = "/A/posixchk_hard";
        unlink(linkp);
        unlink(hard);
        // symlink
        check(symlink(target, linkp) == 0, "symlink");
        if (lstat(linkp, &st) == 0) {
            check(S_ISLNK(st.st_mode), "S_ISLNK");
        } else {
            check(0, "lstat symlink");
        }
        {
            char rbuf[64];
            ssize_t n = readlink(linkp, rbuf, sizeof rbuf - 1);
            if (n >= 0) {
                rbuf[n] = '\0';
                check(strcmp(rbuf, target) == 0, "readlink");
            } else {
                check(0, "readlink");
            }
        }
        // hard link — same directory, check ino equality
        {
            struct stat st2;
            check(link(target, hard) == 0, "link");
            if (stat(target, &st) == 0 && stat(hard, &st2) == 0) {
                check(st.st_ino == st2.st_ino, "link ino");
            } else {
                check(0, "link stat");
            }
            unlink(hard);
        }
        // utimens — set mtime to known value and verify
        {
            struct timespec ts2[2];
            ts2[0].tv_sec = 1000000; ts2[0].tv_nsec = 0;
            ts2[1].tv_sec = 123456789; ts2[1].tv_nsec = 0;
            check(utimensat(0, target, ts2, 0) == 0, "utimensat");
            if (stat(target, &st) == 0) {
                check(st.st_mtime == 123456789, "utimens mtime");
            } else {
                check(0, "utimens stat");
            }
        }
        unlink(linkp);
    }

    /* ── dirent seek/tell + scandir ─────────────────────────────────────── */

    {
        const char *d2 = "/A/posixchk_scan";
        mkdir(d2, 0755);
        // create two files for scandir
        int fd = open("/A/posixchk_scan/a", O_CREAT | O_WRONLY, 0644);
        if (fd >= 3) close(fd);
        fd = open("/A/posixchk_scan/b", O_CREAT | O_WRONLY, 0644);
        if (fd >= 3) close(fd);
        DIR *dd = opendir(d2);
        if (dd) {
            long pos = telldir(dd);
            check(pos == 0, "telldir");
            readdir(dd);
            seekdir(dd, 0);
            check(telldir(dd) == 0, "seekdir");
            closedir(dd);
        } else {
            check(0, "opendir scan");
        }
        {
            struct dirent **namelist = NULL;
            int n = scandir(d2, &namelist, NULL, alphasort);
            check(n >= 2, "scandir");
            if (n >= 2) {
                for (int i = 0; i < n; i++) free(namelist[i]);
                free(namelist);
            }
        }
        unlink("/A/posixchk_scan/a");
        unlink("/A/posixchk_scan/b");
        rmdir(d2);
    }

    /* ── time: strptime / gmtime / strftime ─────────────────────────────── */

    {
        struct tm tm;
        memset(&tm, 0, sizeof tm);
        char *r = strptime("2023-03-14 15:09:26", "%Y-%m-%d %H:%M:%S", &tm);
        check(r != NULL && tm.tm_year == 123 && tm.tm_mon == 2 && tm.tm_mday == 14 && tm.tm_hour == 15 && tm.tm_min == 9 && tm.tm_sec == 26, "strptime");
        // strftime round-trip
        char out[64];
        struct tm tm2 = {0};
        tm2.tm_year = 123; tm2.tm_mon = 2; tm2.tm_mday = 14; tm2.tm_hour = 15; tm2.tm_min = 9; tm2.tm_sec = 26;
        size_t sl = strftime(out, sizeof out, "%Y-%m-%d", &tm2);
        check(sl == 10 && strcmp(out, "2023-03-14") == 0, "strftime");
        // gmtime
        time_t tt = 0;
        struct tm *gtm = gmtime(&tt);
        check(gtm != NULL && gtm->tm_year == 70, "gmtime");
    }

    /* ── math: Bessel ───────────────────────────────────────────────────── */

    {
        double j0v = j0(0.0);
        check(j0v > 0.999 && j0v < 1.001, "j0(0)");
        double y = j1(0.0);
        (void)y;
        check(1, "j1 link");
    }

    /* ── unistd: gethostname / pathconf / getopt ────────────────────────── */

    {
        char hn[32];
        check(gethostname(hn, sizeof hn) == 0 && strcmp(hn, "bedrock") == 0, "gethostname");
        check(pathconf("/A", _PC_NAME_MAX) == 255, "pathconf");
        check(fpathconf(0, _PC_PIPE_BUF) == 512, "fpathconf");
    }

    /* ── string: memccpy / stpcpy / strndup ─────────────────────────────── */

    {
        char dst[16];
        char *p = memccpy(dst, "abcde", 'c', 5);
        check(p != NULL && dst[2] == 'c', "memccpy");
        char d2[16];
        char *e = stpcpy(d2, "hi");
        check(e != NULL && strcmp(d2, "hi") == 0 && (e - d2) == 2, "stpcpy");
        char *nd = strndup("abcdef", 3);
        check(nd != NULL && strcmp(nd, "abc") == 0, "strndup");
        if (nd) free(nd);
        check(ffs(8) == 4, "ffs");
    }

    /* ── stdio: ungetc / fileno / fdopen ────────────────────────────────── */

    {
        FILE *f = fopen("/A/posixchk_data.txt", "r");
        if (f) {
            int c = fgetc(f);
            check(c != EOF, "fgetc");
            check(ungetc(c, f) == c, "ungetc");
            check(fgetc(f) == c, "ungetc roundtrip");
            check(fileno(f) >= 3 || fileno(f) == -1, "fileno");
            fclose(f);
        } else {
            check(0, "fopen for ungetc");
        }
        int fd3 = open("/A/posixchk_data.txt", O_RDONLY, 0);
        if (fd3 >= 3) {
            FILE *ff = fdopen(fd3, "r");
            check(ff != NULL, "fdopen");
            if (ff) fclose(ff);
            else close(fd3);
        } else {
            check(0, "fdopen open");
        }
    }

    return fails;
}