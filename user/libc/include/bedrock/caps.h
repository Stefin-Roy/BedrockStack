#pragma once
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define BEDROCK_R  1
#define BEDROCK_RW 3

typedef struct {
    char *path;   /* NUL-terminated, owned after list */
    char *method; /* NULL if object cap, else NUL-terminated */
    uint32_t perm; /* BEDROCK_R or BEDROCK_RW */
} bedrock_cap_t;

/* Number of caps on /proc/self/caps or -errno */
int bedrock_caps_count(void);

/* 1 if caller has (path,method) with at least `perm` (BEDROCK_R/RW), 0 if not, -errno on read error */
int bedrock_has_cap(const char *path, const char *method, uint32_t perm);

/* Write up to `cap` caps into `out` (path/method strdup'd). Returns count or -errno.
   Caller must free each entry via bedrock_caps_free. */
int bedrock_caps_list(bedrock_cap_t *out, size_t cap);

/* Free list allocated by bedrock_caps_list */
void bedrock_caps_free(bedrock_cap_t *list, size_t n);

#ifdef __cplusplus
}
#endif
