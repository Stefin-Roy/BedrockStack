/*
 * BedrockOS permissive libc — <ctype.h>
 *
 * Implemented in Rust (ctype.rs).  All functions tolerate EOF (-1) as input.
 */
#ifndef BEDROCK_LIBC_CTYPE_H
#define BEDROCK_LIBC_CTYPE_H

#ifdef __cplusplus
extern "C" {
#endif

int isalnum(int c);
int isalpha(int c);
int isascii(int c);
int isblank(int c);
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
int toascii(int c);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_CTYPE_H */