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

#include "m_misc.h"
#include "w_file.h"
#include "z_zone.h"
#include <errno.h>

typedef struct
{
    wad_file_t wad;
    FILE *fstream;
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
    FILE *fstream;
    long length;
    byte *buf;
    size_t got;

    wmem_log("[w_mem] open ");
    wmem_log(path);
    wmem_log("\n");
    fstream = fopen(path, "rb");

    if (fstream == NULL)
    {
        wmem_log("[w_mem] fopen failed errno=");
        // crude decimal of errno
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

    length = M_FileLength(fstream);

    if (length <= 0)
    {
        wmem_log("[w_mem] M_FileLength <=0\n");
        fclose(fstream);
        return NULL;
    }

    {
        char tmp[64];
        // log length
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
        fclose(fstream);
        return NULL;
    }

    got = fread(buf, 1, (size_t)length, fstream);

    if (got != (size_t)length)
    {
        wmem_log("[w_mem] fread short\n");
        free(buf);
        fclose(fstream);
        return NULL;
    }
    wmem_log("[w_mem] open ok\n");

    result = Z_Malloc(sizeof(mem_wad_file_t), PU_STATIC, 0);
    result->wad.file_class = &mem_wad_file;
    result->wad.mapped = buf;
    result->wad.length = (unsigned int)length;
    result->fstream = fstream;

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

    fclose(mem_wad->fstream);
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