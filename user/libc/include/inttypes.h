/*
 * BedrockOS permissive libc — <inttypes.h>
 */
#ifndef BEDROCK_LIBC_INTTYPES_H
#define BEDROCK_LIBC_INTTYPES_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int64_t  intmax_t;
typedef uint64_t uintmax_t;

#define PRId8 "d"
#define PRId16 "d"
#define PRId32 "d"
#define PRId64 "ld"
#define PRIi8 "i"
#define PRIi16 "i"
#define PRIi32 "i"
#define PRIi64 "li"
#define PRIu8 "u"
#define PRIu16 "u"
#define PRIu32 "u"
#define PRIu64 "lu"
#define PRIo8 "o"
#define PRIo16 "o"
#define PRIo32 "o"
#define PRIo64 "lo"
#define PRIx8 "x"
#define PRIx16 "x"
#define PRIx32 "x"
#define PRIx64 "lx"
#define PRIX8 "X"
#define PRIX16 "X"
#define PRIX32 "X"
#define PRIX64 "lX"
#define PRIdLEAST8 "d"
#define PRIdLEAST16 "d"
#define PRIdLEAST32 "d"
#define PRIdLEAST64 "ld"
#define PRIdFAST8 "d"
#define PRIdFAST16 "ld"
#define PRIdFAST32 "ld"
#define PRIdFAST64 "ld"
#define PRIdMAX "ld"
#define PRIuMAX "lu"
#define PRIxMAX "lx"
#define SCNd8 "hhd"
#define SCNd16 "hd"
#define SCNd32 "d"
#define SCNd64 "ld"
#define SCNi8 "hhi"
#define SCNi16 "hi"
#define SCNi32 "i"
#define SCNi64 "li"
#define SCNu8 "hhu"
#define SCNu16 "hu"
#define SCNu32 "u"
#define SCNu64 "lu"
#define SCNo8 "hho"
#define SCNo16 "ho"
#define SCNo32 "o"
#define SCNo64 "lo"
#define SCNx8 "hhx"
#define SCNx16 "hx"
#define SCNx32 "x"
#define SCNx64 "lx"

intmax_t imaxabs(intmax_t j);
typedef struct { intmax_t quot, rem; } imaxdiv_t;
imaxdiv_t imaxdiv(intmax_t numer, intmax_t denom);
intmax_t strtoimax(const char *s, char **endptr, int base);
uintmax_t strtoumax(const char *s, char **endptr, int base);
intmax_t wcstoimax(const wchar_t *s, wchar_t **endptr, int base);
uintmax_t wcstoumax(const wchar_t *s, wchar_t **endptr, int base);

#ifdef __cplusplus
}
#endif

#endif /* BEDROCK_LIBC_INTTYPES_H */