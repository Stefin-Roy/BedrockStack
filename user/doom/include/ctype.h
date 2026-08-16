/*
 * Minimal freestanding <ctype.h> for the vendored doomgeneric engine.
 * Implemented as real functions in shim.c (gcc is built with -fno-builtin).
 * All take/return int and must tolerate EOF == -1.
 */
#ifndef BEDROCK_CTYPE_H
#define BEDROCK_CTYPE_H

#ifdef __cplusplus
extern "C" {
#endif

int isalnum(int c);
int isalpha(int c);
int iscntrl(int c);
int isdigit(int c);
int isgraph(int c);
int islower(int c);
int isprint(int c);
int ispunct(int c);
int isspace(int c);
int isupper(int c);
int isxdigit(int c);
int tolower(int c);
int toupper(int c);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_CTYPE_H */
