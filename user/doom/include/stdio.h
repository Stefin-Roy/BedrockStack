/*
 * Minimal freestanding <stdio.h> for the vendored doomgeneric engine.
 *
 * Only the subset the engine actually uses is declared.  The FILE type is
 * opaque: fopen/fclose/fread/fwrite/fseek/ftell/fflush are implemented by
 * `user/libc` (Rust), everything else (sprintf/snprintf/vsnprintf/fprintf/
 * vfprintf/feof/fscanf, plus the stdout/stderr handles) is implemented in
 * `user/doom/platform/shim.c`.
 *
 * Part of the BedrockOS DOOM port (GPL-2.0+ by linkage).  This header is
 * intentionally isolated from the permissively-licensed user/libc crate.
 */
#ifndef BEDROCK_STDIO_H
#define BEDROCK_STDIO_H

#include <stddef.h>
#include <stdarg.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct FILE FILE;

#define EOF (-1)

#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2

extern FILE *stdin;
extern FILE *stdout;
extern FILE *stderr;

int  printf(const char *fmt, ...);
int  sprintf(char *buf, const char *fmt, ...);
int  snprintf(char *buf, size_t size, const char *fmt, ...);
int  vsnprintf(char *buf, size_t size, const char *fmt, va_list ap);
int  fprintf(FILE *f, const char *fmt, ...);
int  vfprintf(FILE *f, const char *fmt, va_list ap);
int  fflush(FILE *f);

FILE *fopen(const char *path, const char *mode);
int   fclose(FILE *f);
size_t fread(void *ptr, size_t size, size_t nmemb, FILE *f);
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *f);
int   fseek(FILE *f, long offset, int whence);
long  ftell(FILE *f);
int   feof(FILE *f);
int   fscanf(FILE *f, const char *fmt, ...);

int putchar(int c);
int puts(const char *s);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_STDIO_H */
