/*
 * shim.c — freestanding C runtime shims for the vendored doomgeneric engine.
 *
 * Part of the BedrockOS DOOM port.  The engine is GPL-2.0+ by linkage; this
 * file is our own glue, kept deliberately separate from `user/libc` (which is
 * not GPL).  It provides only what the engine needs that `user/libc` does not
 * already export:
 *
 *   exit, sprintf/snprintf/vsnprintf/fprintf/vfprintf, feof, fscanf,
 *   abs, labs, atof, fabs, sin, cos, ctype, strcasecmp, strncasecmp,
 *   getenv, system, remove, rename, mkdir, qsort, and the stdin/stdout/stderr
 *   handles.
 *
 * IMPORTANT symbol rules (duplicate-symbol errors are fatal in lld):
 *   * fopen/fclose/fread/fwrite/fseek/ftell/fflush, printf/puts/putchar,
 *     malloc/calloc/realloc/free, strlen/strcmp/strncpy/strcpy/strcat/
 *     strchr/strrchr/strstr/memchr/atoi/strtol/strdup, write/read and
 *     __errno_location come from `user/libc` (Rust) — NEVER redefine them here.
 *   * memset/memcpy/memmove/memcmp come from Rust compiler-builtins (the
 *     `compiler-builtins-mem` build-std feature) — NEVER redefine them here.
 *
 * Quirk: libc's fseek has no SEEK_END, so M_FileLength() (engine) cannot size
 * an IWAD; that is a documented runtime limitation of the port, not a link
 * problem, and it is fixed outside this crate.
 */

#include <stddef.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <ctype.h>
#include <math.h>
#include <errno.h>
#include <limits.h>

/* ── Rust bridge (defined in src/main.rs) ───────────────────────────── */

extern int32_t  bedrock_fb_mode(void *out);
extern intptr_t bedrock_fb_write(uint64_t offset, const void *src, size_t len);
extern uint64_t bedrock_now_ns(void);
extern intptr_t bedrock_sleep_ms(uint64_t ms);
extern intptr_t bedrock_read_events(void *buf, size_t cap);
extern intptr_t bedrock_serial(const void *src, size_t len);
extern void     bedrock_exit(int32_t code) __attribute__((noreturn));

/* ── standard stream handles ────────────────────────────────────────── */

/* The engine references stdout/stderr (fflush(stdout), fprintf(stderr, ...)).
 * Keep them NULL: every libc FILE* entry point null-checks before touching
 * the FILE, so accidental use is a safe no-op.  My fprintf treats a NULL
 * handle (== stdout/stderr here) as "route to COM1 via bedrock_serial". */
FILE *stdin  = NULL;
FILE *stdout = NULL;
FILE *stderr = NULL;

/* ── exit / abort ───────────────────────────────────────────────────── */

void exit(int code) __attribute__((noreturn));
void exit(int code)
{
    bedrock_exit(code);
}

void abort(void) __attribute__((noreturn));
void abort(void)
{
    bedrock_exit(-1);
}

/* ── printf-family formatting core ──────────────────────────────────── */

typedef struct fmt_out
{
    char  *buf;
    size_t cap;   /* 0 means "never write" (only count) */
    size_t want;  /* logical (would-be) length so far */
} fmt_out;

static void fmt_putc(fmt_out *o, char c)
{
    o->want++;
    if (o->cap != 0 && o->want <= o->cap)
    {
        o->buf[o->want - 1] = c;
    }
}

static void fmt_puts(fmt_out *o, const char *s, size_t n)
{
    size_t i;
    for (i = 0; i < n; i++)
    {
        fmt_putc(o, s[i]);
    }
}

static void fmt_repeat(fmt_out *o, char c, size_t n)
{
    while (n--)
    {
        fmt_putc(o, c);
    }
}

static size_t u64_to_str(uint64_t v, char *buf)
{
    char tmp[24];
    size_t n = 0;
    size_t i;
    if (v == 0)
    {
        tmp[n++] = '0';
    }
    while (v != 0)
    {
        tmp[n++] = (char)('0' + v % 10);
        v /= 10;
    }
    for (i = 0; i < n; i++)
    {
        buf[i] = tmp[n - 1 - i];
    }
    return n;
}

static size_t digits_count(uint64_t v, uint64_t base)
{
    size_t n = 1;
    while (v >= base)
    {
        v /= base;
        n++;
    }
    return n;
}

static void fmt_u64(fmt_out *o, uint64_t v, unsigned base, int upper)
{
    char digits[64];
    size_t n = 0;
    size_t i;
    if (v == 0)
    {
        digits[n++] = '0';
    }
    while (v != 0)
    {
        unsigned d = (unsigned)(v % base);
        digits[n++] = d < 10 ? (char)('0' + d)
                             : (char)((upper ? 'A' : 'a') + (d - 10));
        v /= base;
    }
    for (i = n; i > 0; i--)
    {
        fmt_putc(o, digits[i - 1]);
    }
}

typedef struct conv_spec
{
    int  left;
    int  zero;
    int  alt;
    int  width;
    int  prec_set;
    int  prec;
} conv_spec;

/* Parse flags/width/precision; leaves *pp pointing at the spec char. */
static void fmt_parse_spec(const char **pp, conv_spec *c)
{
    const char *p = *pp;
    c->left = 0;
    c->zero = 0;
    c->alt  = 0;
    c->width = 0;
    c->prec_set = 0;
    c->prec = 0;
    for (;;)
    {
        if (*p == '-')     { c->left = 1; p++; }
        else if (*p == '0'){ c->zero = 1; p++; }
        else if (*p == '+' || *p == ' ' || *p == '#') { c->alt = 1; p++; }
        else break;
    }
    while (*p >= '0' && *p <= '9')
    {
        c->width = c->width * 10 + (*p - '0');
        p++;
    }
    if (*p == '.')
    {
        p++;
        c->prec_set = 1;
        while (*p >= '0' && *p <= '9')
        {
            c->prec = c->prec * 10 + (*p - '0');
            p++;
        }
    }
    *pp = p;
}

static void fmt_signed(fmt_out *o, conv_spec *c, long long v, int base)
{
    int neg = v < 0;
    uint64_t mag = neg ? (uint64_t)(-(v + 1)) + 1 : (uint64_t)v;
    size_t dlen = digits_count(mag, (uint64_t)base);
    size_t pn = (size_t)(c->prec_set && c->prec > (int)dlen ? c->prec : (int)dlen);
    int tot = (int)pn + (neg ? 1 : 0);

    if (!c->left && c->width > tot && !(c->zero && !c->prec_set))
    {
        fmt_repeat(o, ' ', (size_t)(c->width - tot));
    }
    if (neg)
    {
        fmt_putc(o, '-');
    }
    if (!c->left && c->zero && !c->prec_set && c->width > tot)
    {
        fmt_repeat(o, '0', (size_t)(c->width - tot));
    }
    fmt_repeat(o, '0', pn - dlen);
    fmt_u64(o, mag, (unsigned)base, 0);
    if (c->left && c->width > tot)
    {
        fmt_repeat(o, ' ', (size_t)(c->width - tot));
    }
}

static void fmt_unsigned(fmt_out *o, conv_spec *c, uint64_t v, int base, int upper)
{
    size_t dlen = digits_count(v, (uint64_t)base);
    size_t pn = (size_t)(c->prec_set && c->prec > (int)dlen ? c->prec : (int)dlen);

    if (!c->left && c->width > (int)pn && !(c->zero && !c->prec_set))
    {
        fmt_repeat(o, ' ', (size_t)(c->width - (int)pn));
    }
    if (!c->left && c->zero && !c->prec_set && c->width > (int)pn)
    {
        fmt_repeat(o, '0', (size_t)(c->width - (int)pn));
    }
    fmt_repeat(o, '0', pn - dlen);
    fmt_u64(o, v, (unsigned)base, upper);
    if (c->left && c->width > (int)pn)
    {
        fmt_repeat(o, ' ', (size_t)(c->width - (int)pn));
    }
}

/* Build "sign+int.frac" for %f into raw; returns length. */
static size_t fmt_float_raw(char *raw, double v, int prec)
{
    size_t n = 0;
    if (v < 0)
    {
        raw[n++] = '-';
        v = -v;
    }
    uint64_t sc = 1;
    int i;
    for (i = 0; i < prec; i++)
    {
        sc *= 10;
    }
    uint64_t ip, fp;
    if (v >= (double)UINT64_MAX / (double)sc)
    {
        ip = (uint64_t)v;
        fp = 0;
    }
    else
    {
        uint64_t scv = (uint64_t)(v * (double)sc + 0.5);
        ip = scv / sc;
        fp = scv % sc;
    }
    n += u64_to_str(ip, raw + n);
    if (prec > 0)
    {
        char f[24];
        size_t fn = 0;
        raw[n++] = '.';
        if (fp == 0)
        {
            f[fn++] = '0';
        }
        while (fp != 0)
        {
            f[fn++] = (char)('0' + fp % 10);
            fp /= 10;
        }
        while (fn < (size_t)prec)
        {
            f[fn++] = '0';   /* appended = most-significant position */
        }
        while (fn > 0)
        {
            raw[n++] = f[--fn];
        }
    }
    return n;
}

int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap)
{
    fmt_out o;
    const char *p = fmt;
    o.buf = buf;
    o.cap = size;
    o.want = 0;

    while (*p != '\0')
    {
        if (*p != '%')
        {
            fmt_putc(&o, *p);
            p++;
            continue;
        }
        p++;
        if (*p == '%')
        {
            fmt_putc(&o, '%');
            p++;
            continue;
        }
        conv_spec c;
        fmt_parse_spec(&p, &c);
        /* length modifiers (rare in this engine, support for safety) */
        int ll = 0;
        if (*p == 'l')
        {
            ll = 1;
            p++;
            if (*p == 'l')
            {
                ll = 2;
                p++;
            }
        }
        else if (*p == 'h')
        {
            p++;
            if (*p == 'h')
            {
                p++;
            }
        }
        char spec = *p;
        if (spec == '\0')
        {
            break;
        }
        p++;
        switch (spec)
        {
        case 'c':
        {
            int v = va_arg(ap, int);
            if (!c.left && c.width > 1)
            {
                fmt_repeat(&o, ' ', (size_t)(c.width - 1));
            }
            fmt_putc(&o, (char)(v & 0xFF));
            if (c.left && c.width > 1)
            {
                fmt_repeat(&o, ' ', (size_t)(c.width - 1));
            }
            break;
        }
        case 's':
        {
            const char *v = va_arg(ap, const char *);
            size_t len;
            if (v == NULL)
            {
                v = "(null)";
            }
            len = strlen(v);
            if (c.prec_set && c.prec >= 0 && (size_t)c.prec < len)
            {
                len = (size_t)c.prec;
            }
            if (!c.left && c.width > (int)len)
            {
                fmt_repeat(&o, ' ', (size_t)(c.width - (int)len));
            }
            fmt_puts(&o, v, len);
            if (c.left && c.width > (int)len)
            {
                fmt_repeat(&o, ' ', (size_t)(c.width - (int)len));
            }
            break;
        }
        case 'd':
        case 'i':
        {
            long long v;
            if (ll == 2)      v = va_arg(ap, long long);
            else if (ll == 1) v = (long long)va_arg(ap, long);
            else              v = (long long)va_arg(ap, int);
            fmt_signed(&o, &c, v, 10);
            break;
        }
        case 'u':
        {
            uint64_t v;
            if (ll == 2)      v = (uint64_t)va_arg(ap, unsigned long long);
            else if (ll == 1) v = (uint64_t)va_arg(ap, unsigned long);
            else              v = (uint64_t)va_arg(ap, unsigned int);
            fmt_unsigned(&o, &c, v, 10, 0);
            break;
        }
        case 'o':
        {
            uint64_t v;
            if (ll == 2)      v = (uint64_t)va_arg(ap, unsigned long long);
            else if (ll == 1) v = (uint64_t)va_arg(ap, unsigned long);
            else              v = (uint64_t)va_arg(ap, unsigned int);
            fmt_unsigned(&o, &c, v, 8, 0);
            break;
        }
        case 'x':
        case 'X':
        {
            uint64_t v;
            if (ll == 2)      v = (uint64_t)va_arg(ap, unsigned long long);
            else if (ll == 1) v = (uint64_t)va_arg(ap, unsigned long);
            else              v = (uint64_t)va_arg(ap, unsigned int);
            fmt_unsigned(&o, &c, v, 16, spec == 'X');
            break;
        }
        case 'p':
        {
            void *v = va_arg(ap, void *);
            if (!c.left && c.width > 0)
            {
                fmt_repeat(&o, ' ', (size_t)c.width);
            }
            fmt_puts(&o, "0x", 2);
            fmt_u64(&o, (uintptr_t)v, 16, 0);
            break;
        }
        case 'f':
        {
            double v = va_arg(ap, double);
            int prec = c.prec_set ? c.prec : 6;
            char raw[64];
            size_t n;
            if (prec > 17)
            {
                prec = 17;
            }
            if (v != v)               /* NaN */
            {
                fmt_puts(&o, "nan", 3);
                break;
            }
            n = fmt_float_raw(raw, v, prec);
            if (!c.left && c.width > (int)n)
            {
                fmt_repeat(&o, ' ', (size_t)(c.width - (int)n));
            }
            fmt_puts(&o, raw, n);
            if (c.left && c.width > (int)n)
            {
                fmt_repeat(&o, ' ', (size_t)(c.width - (int)n));
            }
            break;
        }
        default:
            fmt_putc(&o, '%');
            fmt_putc(&o, spec);
            break;
        }
    }

    if (size != 0)
    {
        if (o.want < size)
        {
            buf[o.want] = '\0';
        }
        else
        {
            buf[size - 1] = '\0';
        }
    }
    return (int)o.want;
}

int snprintf(char *buf, size_t size, const char *fmt, ...)
{
    va_list ap;
    int r;
    va_start(ap, fmt);
    r = vsnprintf(buf, size, fmt, ap);
    va_end(ap);
    return r;
}

int sprintf(char *buf, const char *fmt, ...)
{
    va_list ap;
    int r;
    va_start(ap, fmt);
    r = vsnprintf(buf, SIZE_MAX, fmt, ap);
    va_end(ap);
    return r;
}

int vfprintf(FILE *f, const char *fmt, va_list ap)
{
    char buf[4096];
    int n = vsnprintf(buf, sizeof(buf), fmt, ap);
    if (n < 0)
    {
        n = 0;
    }
    if ((size_t)n >= sizeof(buf))
    {
        n = (int)sizeof(buf) - 1;
    }
    if (f == NULL || f == stdout || f == stderr)
    {
        if (n > 0)
        {
            bedrock_serial(buf, (size_t)n);
        }
    }
    else if (n > 0)
    {
        fwrite(buf, 1, (size_t)n, f);
    }
    return n;
}

int fprintf(FILE *f, const char *fmt, ...)
{
    va_list ap;
    int r;
    va_start(ap, fmt);
    r = vfprintf(f, fmt, ap);
    va_end(ap);
    return r;
}

/* ── feof / fscanf ──────────────────────────────────────────────────── */

/*
 * libc's FILE is opaque and cannot carry an EOF flag, so fscanf/feof keep a
 * small side table keyed by the FILE* handle.  A fresh fscanf clears the flag
 * for that handle; the flag is set the moment a read from the handle hits EOF.
 * Single-threaded game, at most a handful of open files — a linear table is
 * fine and the reuse case (fclose + fopen reusing a slot) is handled because
 * every fscanf clears the flag before reading.
 */

#define MAX_SCAN_FILES 16

typedef struct file_state
{
    FILE *f;
    int   eof;
} file_state;

static file_state g_fstates[MAX_SCAN_FILES];
static int g_nfstates = 0;

static void fs_clear(FILE *f)
{
    int i;
    for (i = 0; i < g_nfstates; i++)
    {
        if (g_fstates[i].f == f)
        {
            g_fstates[i].eof = 0;
            return;
        }
    }
}

static void fs_mark_eof(FILE *f)
{
    int i;
    for (i = 0; i < g_nfstates; i++)
    {
        if (g_fstates[i].f == f)
        {
            g_fstates[i].eof = 1;
            return;
        }
    }
    if (g_nfstates < MAX_SCAN_FILES)
    {
        g_fstates[g_nfstates].f = f;
        g_fstates[g_nfstates].eof = 1;
        g_nfstates++;
    }
}

int feof(FILE *f)
{
    int i;
    for (i = 0; i < g_nfstates; i++)
    {
        if (g_fstates[i].f == f)
        {
            return g_fstates[i].eof;
        }
    }
    return 0;
}

typedef struct scan_src
{
    const char *str;   /* non-NULL: string source (sscanf) */
    size_t      pos;
    FILE       *f;     /* FILE source (fscanf) */
    int         back;  /* single pushback byte for FILE mode */
} scan_src;

static int sc_get(scan_src *s)
{
    if (s->back >= 0)
    {
        int c = s->back;
        s->back = -1;
        return c;
    }
    if (s->str != NULL)
    {
        int c = (unsigned char)s->str[s->pos];
        if (c == 0)
        {
            return EOF;
        }
        s->pos++;
        return c;
    }
    else
    {
        unsigned char c;
        if (fread(&c, 1, 1, s->f) == 1)
        {
            return (int)c;
        }
        fs_mark_eof(s->f);
        return EOF;
    }
}

static void sc_unget(scan_src *s, int c)
{
    if (c == EOF)
    {
        return;
    }
    if (s->str != NULL)
    {
        if (s->pos > 0)
        {
            s->pos--;
        }
    }
    else
    {
        s->back = c;
    }
}

static int sc_peek(scan_src *s)
{
    int c = sc_get(s);
    sc_unget(s, c);
    return c;
}

/* Consume a run of whitespace; returns 1 if any was consumed. */
static int sc_skip_ws(scan_src *s)
{
    int c;
    int any = 0;
    for (;;)
    {
        c = sc_peek(s);
        if (c == EOF || !isspace((unsigned char)c))
        {
            break;
        }
        sc_get(s);
        any = 1;
    }
    return any;
}

/*
 * Read a (signed) integer.  base: 0 = auto (%i), else 10/8/16.
 * Returns 1 on success, 0 on matching failure, -1 on input (EOF) failure.
 */
static int scan_int(scan_src *s, int base, long long *out)
{
    int c;
    int neg = 0;
    int radix = base;
    long long v = 0;
    int ndigits = 0;

    sc_skip_ws(s);
    c = sc_peek(s);
    if (c == EOF)
    {
        return -1;
    }
    c = sc_peek(s);
    if (c == '+' || c == '-')
    {
        sc_get(s);
        neg = (c == '-');
    }
    if (radix == 0)
    {
        c = sc_peek(s);
        if (c == '0')
        {
            sc_get(s);
            ndigits = 1;
            c = sc_peek(s);
            if (c == 'x' || c == 'X')
            {
                sc_get(s);
                radix = 16;
                ndigits = 0;
            }
            else
            {
                radix = 8;
            }
        }
        else
        {
            radix = 10;
        }
    }
    for (;;)
    {
        int d = -1;
        c = sc_peek(s);
        if (c >= '0' && c <= '9')
        {
            d = c - '0';
        }
        else if (c >= 'a' && c <= 'f')
        {
            d = c - 'a' + 10;
        }
        else if (c >= 'A' && c <= 'F')
        {
            d = c - 'A' + 10;
        }
        if (d < 0 || d >= radix)
        {
            break;
        }
        sc_get(s);
        v = v * radix + d;
        ndigits++;
    }
    if (ndigits == 0)
    {
        return 0;
    }
    *out = neg ? -v : v;
    return 1;
}

static int scan_v(scan_src *s, const char *fmt, va_list ap)
{
    const char *p = fmt;
    int assigned = 0;

    while (*p != '\0')
    {
        if (isspace((unsigned char)*p))
        {
            sc_skip_ws(s);
            p++;
            continue;
        }
        if (*p != '%')
        {
            int c = sc_peek(s);
            if (c == EOF)
            {
                return assigned == 0 ? EOF : assigned;
            }
            if (c != *p)
            {
                return assigned;
            }
            sc_get(s);
            p++;
            continue;
        }
        p++;
        if (*p == '%')
        {
            int c = sc_peek(s);
            if (c == EOF)
            {
                return assigned == 0 ? EOF : assigned;
            }
            if (c != '%')
            {
                return assigned;
            }
            sc_get(s);
            p++;
            continue;
        }

        int suppress = 0;
        int width = 0;
        char conv;

        if (*p == '*')
        {
            suppress = 1;
            p++;
        }
        while (*p >= '0' && *p <= '9')
        {
            width = width * 10 + (*p - '0');
            p++;
        }
        if (*p == 'h')
        {
            p++;
            if (*p == 'h')
            {
                p++;
            }
        }
        else if (*p == 'l')
        {
            p++;
            if (*p == 'l')
            {
                p++;
            }
        }
        conv = *p;
        if (conv == '\0')
        {
            return assigned;
        }
        p++;

        switch (conv)
        {
        case 's':
        {
            char *dst = suppress ? NULL : va_arg(ap, char *);
            int c;
            size_t n = 0;
            int max = width > 0 ? width : INT_MAX;
            sc_skip_ws(s);
            c = sc_peek(s);
            if (c == EOF)
            {
                return assigned == 0 ? EOF : assigned;
            }
            for (;;)
            {
                c = sc_peek(s);
                if (c == EOF || isspace((unsigned char)c))
                {
                    break;
                }
                sc_get(s);
                if (dst != NULL)
                {
                    dst[n] = (char)c;
                }
                n++;
                if (n >= (size_t)max)
                {
                    break;
                }
            }
            if (n == 0)
            {
                return assigned;
            }
            if (dst != NULL)
            {
                dst[n] = '\0';
            }
            if (!suppress)
            {
                assigned++;
            }
            break;
        }
        case '[':
        {
            char set[256];
            int setn = 0;
            int invert = 0;
            char *dst = suppress ? NULL : va_arg(ap, char *);
            int c;
            size_t n = 0;
            int max = width > 0 ? width : INT_MAX;

            if (*p == '^')
            {
                invert = 1;
                p++;
            }
            if (*p == ']')
            {
                set[setn++] = ']';
                p++;
            }
            while (*p != '\0' && *p != ']')
            {
                set[setn++] = *p;
                p++;
            }
            if (*p == ']')
            {
                p++;
            }
            for (;;)
            {
                int matched;
                int i;
                c = sc_peek(s);
                if (c == EOF || n >= (size_t)max)
                {
                    break;
                }
                matched = invert;
                for (i = 0; i < setn; i++)
                {
                    if ((unsigned char)set[i] == (unsigned char)c)
                    {
                        matched = invert ? 0 : 1;
                        break;
                    }
                }
                if (!matched)
                {
                    break;
                }
                sc_get(s);
                if (dst != NULL)
                {
                    dst[n] = (char)c;
                }
                n++;
            }
            if (n == 0)
            {
                return assigned;
            }
            if (dst != NULL)
            {
                dst[n] = '\0';
            }
            if (!suppress)
            {
                assigned++;
            }
            break;
        }
        case 'c':
        {
            char *dst = suppress ? NULL : va_arg(ap, char *);
            int max = width > 0 ? width : 1;
            int c;
            int n = 0;
            for (;;)
            {
                c = sc_get(s);
                if (c == EOF)
                {
                    break;
                }
                if (dst != NULL)
                {
                    dst[n] = (char)c;
                }
                n++;
                if (n >= max)
                {
                    break;
                }
            }
            if (n == 0)
            {
                return assigned == 0 ? EOF : assigned;
            }
            if (!suppress)
            {
                assigned++;
            }
            break;
        }
        case 'd':
        case 'i':
        case 'u':
        case 'o':
        case 'x':
        case 'X':
        {
            long long v;
            int r;
            int base = (conv == 'i') ? 0
                     : (conv == 'x' || conv == 'X') ? 16
                     : (conv == 'o') ? 8 : 10;
            r = scan_int(s, base, &v);
            if (r == -1)
            {
                return assigned == 0 ? EOF : assigned;
            }
            if (r == 0)
            {
                return assigned;
            }
            if (!suppress)
            {
                if (conv == 'u' || conv == 'x' || conv == 'X' || conv == 'o')
                {
                    unsigned int *dst = va_arg(ap, unsigned int *);
                    *dst = (unsigned int)v;
                }
                else
                {
                    int *dst = va_arg(ap, int *);
                    *dst = (int)v;
                }
                assigned++;
            }
            break;
        }
        case 'n':
        {
            int *dst = suppress ? NULL : va_arg(ap, int *);
            if (dst != NULL)
            {
                *dst = 0;
            }
            break;
        }
        case 'f':
        case 'e':
        case 'g':
        {
            double *dst = suppress ? NULL : va_arg(ap, double *);
            char tmp[64];
            size_t tn = 0;
            int c;
            sc_skip_ws(s);
            c = sc_peek(s);
            if (c == EOF)
            {
                return assigned == 0 ? EOF : assigned;
            }
            c = sc_peek(s);
            if (c == '+' || c == '-')
            {
                sc_get(s);
                if (tn < sizeof(tmp) - 1)
                {
                    tmp[tn++] = (char)c;
                }
            }
            for (;;)
            {
                c = sc_peek(s);
                if (c < '0' || c > '9')
                {
                    break;
                }
                sc_get(s);
                if (tn < sizeof(tmp) - 1)
                {
                    tmp[tn++] = (char)c;
                }
            }
            c = sc_peek(s);
            if (c == '.')
            {
                sc_get(s);
                if (tn < sizeof(tmp) - 1)
                {
                    tmp[tn++] = '.';
                }
                for (;;)
                {
                    c = sc_peek(s);
                    if (c < '0' || c > '9')
                    {
                        break;
                    }
                    sc_get(s);
                    if (tn < sizeof(tmp) - 1)
                    {
                        tmp[tn++] = (char)c;
                    }
                }
            }
            c = sc_peek(s);
            if (c == 'e' || c == 'E')
            {
                sc_get(s);
                if (tn < sizeof(tmp) - 1)
                {
                    tmp[tn++] = (char)c;
                }
                c = sc_peek(s);
                if (c == '+' || c == '-')
                {
                    sc_get(s);
                    if (tn < sizeof(tmp) - 1)
                    {
                        tmp[tn++] = (char)c;
                    }
                }
                for (;;)
                {
                    c = sc_peek(s);
                    if (c < '0' || c > '9')
                    {
                        break;
                    }
                    sc_get(s);
                    if (tn < sizeof(tmp) - 1)
                    {
                        tmp[tn++] = (char)c;
                    }
                }
            }
            if (tn == 0)
            {
                return assigned;
            }
            tmp[tn] = '\0';
            if (!suppress)
            {
                *dst = atof(tmp);
                assigned++;
            }
            break;
        }
        default:
            return assigned;
        }
    }
    return assigned;
}

int fscanf(FILE *f, const char *fmt, ...)
{
    scan_src s;
    va_list ap;
    int r;
    if (f == NULL)
    {
        return EOF;
    }
    s.str = NULL;
    s.pos = 0;
    s.f = f;
    s.back = -1;
    fs_clear(f);
    va_start(ap, fmt);
    r = scan_v(&s, fmt, ap);
    va_end(ap);
    return r;
}

int sscanf(const char *str, const char *fmt, ...)
{
    scan_src s;
    va_list ap;
    int r;
    s.str = str != NULL ? str : "";
    s.pos = 0;
    s.f = NULL;
    s.back = -1;
    va_start(ap, fmt);
    r = scan_v(&s, fmt, ap);
    va_end(ap);
    return r;
}

/* ── abs / labs ─────────────────────────────────────────────────────── */

int abs(int v)
{
    return v < 0 ? -v : v;
}

long labs(long v)
{
    return v < 0 ? -v : v;
}

/* ── atof ───────────────────────────────────────────────────────────── */

double atof(const char *s)
{
    double v = 0.0;
    double frac = 0.0;
    double scale = 0.1;
    int neg = 0;
    int eneg = 0;
    int e = 0;
    if (s == NULL)
    {
        return 0.0;
    }
    while (*s == ' ' || *s == '\t')
    {
        s++;
    }
    if (*s == '+')
    {
        s++;
    }
    else if (*s == '-')
    {
        neg = 1;
        s++;
    }
    while (*s >= '0' && *s <= '9')
    {
        v = v * 10.0 + (double)(*s - '0');
        s++;
    }
    if (*s == '.')
    {
        s++;
        while (*s >= '0' && *s <= '9')
        {
            frac += (double)(*s - '0') * scale;
            scale *= 0.1;
            s++;
        }
        v += frac;
    }
    if (*s == 'e' || *s == 'E')
    {
        double m = 1.0;
        int i;
        s++;
        if (*s == '+')
        {
            s++;
        }
        else if (*s == '-')
        {
            eneg = 1;
            s++;
        }
        while (*s >= '0' && *s <= '9')
        {
            e = e * 10 + (*s - '0');
            s++;
        }
        for (i = 0; i < e; i++)
        {
            m *= 10.0;
        }
        v = eneg ? v / m : v * m;
    }
    return neg ? -v : v;
}

/* ── math (self-contained; no libm on target) ───────────────────────── */

double fabs(double v)
{
    return v < 0 ? -v : v;
}

static double mod2pi(double v)
{
    const double two_pi = 6.28318530717958647692;
    const double pi = 3.14159265358979323846;
    long long q;
    double r;
    if (v >= 0 && v < two_pi)
    {
        return v;
    }
    q = (long long)(v / two_pi);
    r = v - q * two_pi;
    if (r >= two_pi)
    {
        r -= two_pi;
    }
    else if (r < 0)
    {
        r += two_pi;
    }
    if (r > pi)
    {
        r -= two_pi;
    }
    else if (r < -pi)
    {
        r += two_pi;
    }
    return r;
}

double sin(double v)
{
    double x = mod2pi(v);
    double x2 = x * x;
    double term = x;
    double sum = x;
    int i;
    for (i = 1; i <= 20; i++)
    {
        term = -term * x2 / ((double)((2 * i) * (2 * i + 1)));
        sum += term;
        if (term < 1e-18 && term > -1e-18)
        {
            break;
        }
    }
    return sum;
}

double cos(double v)
{
    return sin(v + 1.57079632679489661923);
}

/* ── ctype ──────────────────────────────────────────────────────────── */

int isdigit(int c)  { return c >= '0' && c <= '9'; }
int isalpha(int c)  { return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z'); }
int isalnum(int c)  { return isdigit(c) || isalpha(c); }
int isspace(int c)  { return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v'; }
int isupper(int c)  { return c >= 'A' && c <= 'Z'; }
int islower(int c)  { return c >= 'a' && c <= 'z'; }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int isprint(int c)  { return c >= 0x20 && c <= 0x7e; }
int isgraph(int c)  { return c >= 0x21 && c <= 0x7e; }
int iscntrl(int c)  { return (c >= 0 && c <= 0x1f) || c == 0x7f; }
int ispunct(int c)  { return isgraph(c) && !isalnum(c); }

int tolower(int c)
{
    return (c >= 'A' && c <= 'Z') ? c + 32 : c;
}

int toupper(int c)
{
    return (c >= 'a' && c <= 'z') ? c - 32 : c;
}

/* ── strings.h ──────────────────────────────────────────────────────── */

int strcasecmp(const char *a, const char *b)
{
    while (*a != '\0' && *b != '\0')
    {
        int ca = tolower((unsigned char)*a);
        int cb = tolower((unsigned char)*b);
        if (ca != cb)
        {
            return ca - cb;
        }
        a++;
        b++;
    }
    return tolower((unsigned char)*a) - tolower((unsigned char)*b);
}

int strncasecmp(const char *a, const char *b, size_t n)
{
    size_t i;
    for (i = 0; i < n; i++)
    {
        int ca = tolower((unsigned char)a[i]);
        int cb = tolower((unsigned char)b[i]);
        if (ca != cb)
        {
            return ca - cb;
        }
        if (a[i] == '\0')
        {
            return 0;
        }
    }
    return 0;
}

/* ── stdlib misc (stubs for a system with no user shell / FS) ───────── */

char *getenv(const char *name)
{
    (void)name;
    return NULL;
}

int system(const char *cmd)
{
    (void)cmd;
    errno = ENOENT;
    return -1;
}

int remove(const char *path)
{
    (void)path;
    errno = ENOENT;
    return -1;
}

int rename(const char *oldpath, const char *newpath)
{
    (void)oldpath;
    (void)newpath;
    errno = ENOENT;
    return -1;
}

/* Engine never depends on the directory actually existing. */
int mkdir(const char *path, int mode)
{
    (void)path;
    (void)mode;
    return 0;
}

static void swap_bytes(char *a, char *b, size_t size)
{
    while (size--)
    {
        char t = *a;
        *a = *b;
        *b = t;
        a++;
        b++;
    }
}

void qsort(void *base, size_t nmemb, size_t size,
           int (*cmp)(const void *, const void *))
{
    char *b = (char *)base;
    size_t i, j;
    if (b == NULL || cmp == NULL)
    {
        return;
    }
    for (i = 1; i < nmemb; i++)
    {
        for (j = i; j > 0; j--)
        {
            char *p = b + (j - 1) * size;
            char *c = b + j * size;
            if (cmp(p, c) <= 0)
            {
                break;
            }
            swap_bytes(p, c, size);
        }
    }
}
