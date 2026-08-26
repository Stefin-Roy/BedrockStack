/*
 * w_mem.c — userspace memory-mapped WAD file class for the BedrockOS DOOM port.
 *
 * Part of the BedrockOS DOOM port.  The engine is GPL-2.0+ by linkage; this
 * file is our own glue, kept deliberately separate from the non-GPL
 * `user/libc` crate.
 *
 * This class is the userspace analogue of Chocolate-Doom's POSIX `-mmap`
 * support: on open, the whole WAD is read into a single `malloc`'d buffer and
 * `wad.mapped` points at it.  The engine's `W_CacheLumpNum` /
 * `W_ReleaseLumpNum` fast paths (see `w_wad.c`) then serve every lump as
 * `mapped + position` pointer arithmetic — zero syscalls and zero disk I/O
 * after the one-time preload.  `Read` (used only for the 12-byte header and
 * the lump directory in `W_AddFile`) is a bounds-checked memcpy.
 *
 * The IWAD is 28.8 MiB, comfortably inside the 256 MiB per-process committed
 * frame budget, so the whole-file buffer is cheap.
 */

#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>

#include "m_misc.h"
#include "w_file.h"
#include "z_zone.h"
#include <errno.h>

typedef struct
{
    wad_file_t wad;
    FILE *fstream;
    int fd;
} mem_wad_file_t;

extern wad_file_class_t mem_wad_file;

extern int bedrock_serial(const void *src, size_t len);
static void wmem_log(const char *msg)
{
    if (msg) bedrock_serial(msg, strlen(msg));
}
static wad_file_t *W_Mem_OpenFile(char *path)
{
    mem_wad_file_t *result;
    int fd;
    long length;
    byte *buf;
    ssize_t got;
    size_t total = 0;

    wmem_log("[w_mem] open ");
    wmem_log(path);
    wmem_log("\n");
    fd = open(path, O_RDONLY);
    if (fd < 0)
    {
        wmem_log("[w_mem] open failed errno=");
        {
            char tmp[32];
            int n = 0;
            int e = errno;
            if (e < 0) e = -e;
            if (e == 0) tmp[n++]='0';
            else { char rev[16]; int r=0; while(e>0){ rev[r++]='0'+(e%10); e/=10; } while(r--) tmp[n++]=rev[r]; }
            tmp[n]=0;
            wmem_log(tmp);
            wmem_log(" path=");
            wmem_log(path);
            wmem_log("\n");
        }
        return NULL;
    }

    length = lseek(fd, 0, SEEK_END);
    if (length <= 0)
    {
        wmem_log("[w_mem] lseek/seek_end <=0\n");
        close(fd);
        return NULL;
    }
    if (lseek(fd, 0, SEEK_SET) < 0)
    {
        wmem_log("[w_mem] lseek set failed\n");
        close(fd);
        return NULL;
    }

    {
        char tmp[64];
        wmem_log("[w_mem] length ");
        {
            long v = length;
            char rev[32]; int r=0, n=0;
            if(v==0) tmp[n++]='0';
            else while(v>0){ rev[r++]='0'+(v%10); v/=10; } while(r--) tmp[n++]=rev[r];
            tmp[n]=0;
            wmem_log(tmp);
            wmem_log("\n");
        }
    }

    buf = (byte *)malloc((size_t)length);
    if (buf == NULL)
    {
        wmem_log("[w_mem] malloc failed\n");
        close(fd);
        return NULL;
    }

    // Chunked fd reads via 1 MiB windows: keeps each read within the
    // 4 MiB (16×252 KiB) batched DMA window and avoids a single 28 MiB
    // IoBuffer that can hit PRDT/EINVAL. Bypasses stdio's double copy
    // and still collapses to ~7 NCQ batches per syscall in the kernel
    // (252 KiB per run, 16 per submit). Falls back to loop on short.
    {
        const size_t CHUNK = 1024 * 1024;
        while (total < (size_t)length)
        {
            size_t want = (size_t)length - total;
            if (want > CHUNK) want = CHUNK;
            got = read(fd, buf + total, want);
            if (got <= 0)
            {
                wmem_log("[w_mem] read failed/short got=");
                {
                    char tmp[64];
                    int n = 0;
                    long v = (long)got;
                    int neg = 0;
                    if (v < 0) { neg = 1; v = -v; }
                    if (neg) tmp[n++] = '-';
                    char rev[32]; int r=0;
                    if (v==0) tmp[n++]='0';
                    else while(v>0){ rev[r++]='0'+(v%10); v/=10; } while(r--) tmp[n++]=rev[r];
                    tmp[n]=0;
                    wmem_log(tmp);
                }
                wmem_log(" errno=");
                {
                    char tmp[32];
                    int n=0;
                    int e = errno;
                    if (e<0) e=-e;
                    if (e==0) tmp[n++]='0';
                    else { char rev[16]; int r=0; while(e>0){ rev[r++]='0'+(e%10); e/=10; } while(r--) tmp[n++]=rev[r]; }
                    tmp[n]=0;
                    wmem_log(tmp);
                }
                wmem_log(" total=");
                {
                    char tmp[32];
                    int n=0;
                    long v = (long)total;
                    char rev[32]; int r=0;
                    if(v==0) tmp[n++]='0';
                    else while(v>0){ rev[r++]='0'+(v%10); v/=10; } while(r--) tmp[n++]=rev[r];
                    tmp[n]=0;
                    wmem_log(tmp);
                }
                wmem_log(" want=");
                {
                    char tmp[32];
                    int n=0;
                    long v = (long)want;
                    char rev[32]; int r=0;
                    if(v==0) tmp[n++]='0';
                    else while(v>0){ rev[r++]='0'+(v%10); v/=10; } while(r--) tmp[n++]=rev[r];
                    tmp[n]=0;
                    wmem_log(tmp);
                }
                wmem_log("\n");
                free(buf);
                close(fd);
                return NULL;
            }
            total += (size_t)got;
            // Short read before EOF – loop will retry; log progress.
            if ((size_t)got != want) {
                wmem_log("[w_mem] short chunk got ");
                {
                    char tmp[32];
                    int n=0;
                    long v = (long)got;
                    char rev[32]; int r=0;
                    if(v==0) tmp[n++]='0';
                    else while(v>0){ rev[r++]='0'+(v%10); v/=10; } while(r--) tmp[n++]=rev[r];
                    tmp[n]=0;
                    wmem_log(tmp);
                }
                wmem_log(" want ");
                {
                    char tmp[32];
                    int n=0;
                    long v = (long)want;
                    char rev[32]; int r=0;
                    if(v==0) tmp[n++]='0';
                    else while(v>0){ rev[r++]='0'+(v%10); v/=10; } while(r--) tmp[n++]=rev[r];
                    tmp[n]=0;
                    wmem_log(tmp);
                }
                wmem_log("\n");
            }
        }
    }
    if (total != (size_t)length)
    {
        wmem_log("[w_mem] read short\n");
        free(buf);
        close(fd);
        return NULL;
    }
    wmem_log("[w_mem] open ok\n");

    result = Z_Malloc(sizeof(mem_wad_file_t), PU_STATIC, 0);
    result->wad.file_class = &mem_wad_file;
    result->wad.mapped = buf;
    result->wad.length = (unsigned int)length;
    result->fstream = NULL;
    result->fd = fd;

    return &result->wad;
}

static void W_Mem_CloseFile(wad_file_t *wad)
{
    mem_wad_file_t *mem_wad;

    mem_wad = (mem_wad_file_t *) wad;

    if (mem_wad->wad.mapped != NULL)
    {
        free(mem_wad->wad.mapped);
        mem_wad->wad.mapped = NULL;
    }

    if (mem_wad->fstream)
        fclose(mem_wad->fstream);
    if (mem_wad->fd >= 0)
        close(mem_wad->fd);
    Z_Free(mem_wad);
}

// Read data from the specified position in the mapped buffer.
// Returns the number of bytes read.

static size_t W_Mem_Read(wad_file_t *wad, unsigned int offset,
                         void *buffer, size_t buffer_len)
{
    if (offset >= wad->length)
    {
        return 0;
    }

    if ((size_t)offset + buffer_len > wad->length)
    {
        buffer_len = wad->length - offset;
    }

    memcpy(buffer, (const byte *)wad->mapped + offset, buffer_len);

    return buffer_len;
}

wad_file_class_t mem_wad_file =
{
    W_Mem_OpenFile,
    W_Mem_CloseFile,
    W_Mem_Read,
};