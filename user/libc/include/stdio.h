/*
 * BedrockOS permissive libc — <stdio.h>
 *
 * Part of the permissively-licensed `user/libc` crate.  Every function is
 * implemented in Rust (stdio.rs / format.rs / scan.rs / fd.rs).  The FILE
 * type is opaque; the three standard handles are task-console streams.
 */
#ifndef BEDROCK_LIBC_STDIO_H
#define BEDROCK_LIBC_STDIO_H

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
int  vprintf(const char *fmt, va_list ap);
int  fprintf(FILE *f, const char *fmt, ...);
int  vfprintf(FILE *f, const char *fmt, va_list ap);
int  sprintf(char *buf, const char *fmt, ...);
int  vsprintf(char *buf, const char *fmt, va_list ap);
int  snprintf(char *buf, size_t size, const char *fmt, ...);
int  vsnprintf(char *buf, size_t size, const char *fmt, va_list ap);

int  scanf(const char *fmt, ...);
int  vscanf(const char *fmt, va_list ap);
int  fscanf(FILE *f, const char *fmt, ...);
int  vfscanf(FILE *f, const char *fmt, va_list ap);
int  sscanf(const char *s, const char *fmt, ...);
int  vsscanf(const char *s, const char *fmt, va_list ap);

FILE *fopen(const char *path, const char *mode);
int   fclose(FILE *f);
int   fflush(FILE *f);
size_t fread(void *ptr, size_t size, size_t nmemb, FILE *f);
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *f);
int   fseek(FILE *f, long offset, int whence);
long  ftell(FILE *f);
void  rewind(FILE *f);

int   feof(FILE *f);
int   ferror(FILE *f);
void  clearerr(FILE *f);

int   fgetc(FILE *f);
int   getc(FILE *f);
int   getchar(void);
int   fputc(int c, FILE *f);
int   putc(int c, FILE *f);
int   putchar(int c);
char *fgets(char *s, int n, FILE *f);
int   fputs(const char *s, FILE *f);
int   puts(const char *s);

int   remove(const char *path);
int   rename(const char *oldpath, const char *newpath);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_STDIO_H */