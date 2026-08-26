/*
 * doomgeneric_bedrock.c — the BedrockOS platform glue for the vendored
 * doomgeneric engine.
 *
 * Part of the BedrockOS DOOM port.  The engine is GPL-2.0+ by linkage; this
 * file is our own glue, deliberately separate from the non-GPL `user/libc`
 * crate.  It implements the six `DG_*` entry points the engine calls:
 *
 *   DG_Init          — query the framebuffer mode, register the quit hook
 *   DG_DrawFrame     — nearest-neighbour scale 640x400 XRGB → native fb,
 *                      convert to the fb's RGB/BGR byte order, push via
 *                      bedrock_fb_write
 *   DG_SleepMs       — bedrock_sleep_ms
 *   DG_GetTicksMs    — bedrock_now_ns / 1e6
 *   DG_GetKey        — drain /input/events, translate Linux KEY_* → Doom keys
 *   DG_SetWindowTitle— no-op (no windows)
 *
 * and `main`, which drives the engine forever and exits cleanly when the
 * in-game quit handler (`I_AtExit`) fires.
 */

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "doomgeneric.h"
#include "doomkeys.h"
#include "i_system.h"

/* ── Rust bridge (defined in src/main.rs) ───────────────────────────── */

extern int32_t  bedrock_fb_mode(void *out);
extern intptr_t bedrock_fb_write(uint64_t offset, const void *src, size_t len);
extern uint64_t bedrock_now_ns(void);
extern intptr_t bedrock_sleep_ms(uint64_t ms);
extern intptr_t bedrock_read_events(void *buf, size_t cap);
extern void     bedrock_exit(int32_t code) __attribute__((noreturn));
extern intptr_t bedrock_serial(const void *src, size_t len);
static void dlog(const char *s) { if(s) bedrock_serial(s, strlen(s)); }

/* ── framebuffer mode (matches src/main.rs BedrockFbMode, 29 bytes) ─── */

typedef struct BedrockFbMode
{
    uint8_t  present;
    uint32_t width;
    uint32_t height;
    uint32_t stride;        /* pixels per scanline */
    uint32_t bpp;           /* bytes per pixel */
    uint32_t pixel_format;  /* 1 = rgb, 2 = bgr */
    uint64_t size;          /* stride * height * bpp */
} __attribute__((packed)) BedrockFbMode;

static BedrockFbMode g_fb;
static int           g_fb_present = 0;

/* ── quit handling ──────────────────────────────────────────────────── */

static int g_quit = 0;

static void DG_QuitHandler(void)
{
    g_quit = 1;
}

/* ── DG_Init ────────────────────────────────────────────────────────── */

void DG_Init(void)
{
    dlog("[doom] DG_Init enter\n");
    int32_t r = bedrock_fb_mode(&g_fb);
    dlog("[doom] bedrock_fb_mode r=");
    {
        char tmp[16]; int v = r; int n=0; char rev[16]; int rr=0;
        if(v==0) tmp[n++]='0'; else { int neg=v<0; if(neg) v=-v; while(v>0){ rev[rr++]='0'+(v%10); v/=10; } if(neg) tmp[n++]='-'; while(rr--) tmp[n++]=rev[rr]; } tmp[n]=0; dlog(tmp); dlog("\n");
    }
    if (r == 0 &&
        g_fb.present != 0 &&
        g_fb.width > 0 && g_fb.height > 0 &&
        g_fb.stride >= g_fb.width && g_fb.bpp > 0)
    {
        g_fb_present = 1;
        dlog("[doom] fb present\n");
    } else {
        dlog("[doom] fb NOT present\n");
    }

    /* The engine's quit path (M_QuitResponse -> I_Quit) returns without
     * exiting when ORIGCODE is off; this flag lets our main loop notice. */
    I_AtExit(DG_QuitHandler, false);
}

/* ── DG_DrawFrame ───────────────────────────────────────────────────── */

void DG_DrawFrame(void)
{
    static uint8_t *frame = NULL;
    static uint64_t frame_bytes = 0;
    static uint32_t *col_lut = NULL;   /* native x  -> source x */
    static uint32_t *row_lut = NULL;   /* native y  -> source y */
    static uint32_t lut_w = 0, lut_h = 0;

    const uint32_t w = g_fb.width;
    const uint32_t h = g_fb.height;
    const uint32_t bpp = g_fb.bpp;
    const uint32_t sw = DOOMGENERIC_RESX;
    const uint32_t sh = DOOMGENERIC_RESY;
    const uint64_t rowbytes = (uint64_t)g_fb.stride * bpp;
    const uint64_t total = rowbytes * h;
    uint32_t y;

    if (!g_fb_present || DG_ScreenBuffer == NULL || w == 0 || h == 0)
    {
        return;
    }

    /* One contiguous native-frame destination; a whole frame is then pushed
     * in a single `/dev/fb` write instead of one syscall per scanline. */
    if (frame == NULL || frame_bytes < total)
    {
        if (frame != NULL)
        {
            free(frame);
        }
        frame = malloc((size_t)total);
        if (frame == NULL)
        {
            return;
        }
        frame_bytes = total;
        /* Padding columns (width..stride) must stay black; zero them once —
         * they are never written again. */
        memset(frame, 0, (size_t)total);
    }

    /* Nearest-neighbour scale LUTs — recomputed only when geometry changes,
     * never per frame. */
    if (col_lut == NULL || lut_w != w)
    {
        uint32_t *nl = (uint32_t *)realloc(col_lut, (size_t)w * sizeof(uint32_t));
        if (nl == NULL)
        {
            return;
        }
        col_lut = nl;
        for (y = 0; y < w; y++)
        {
            col_lut[y] = (y * sw) / w;
        }
        lut_w = w;
    }
    if (row_lut == NULL || lut_h != h)
    {
        uint32_t *nl = (uint32_t *)realloc(row_lut, (size_t)h * sizeof(uint32_t));
        if (nl == NULL)
        {
            return;
        }
        row_lut = nl;
        for (y = 0; y < h; y++)
        {
            row_lut[y] = (y * sh) / h;
        }
        lut_h = h;
    }

    for (y = 0; y < h; y++)
    {
        const pixel_t *srcrow = DG_ScreenBuffer + (size_t)row_lut[y] * sw;
        uint8_t *p = frame + (size_t)y * rowbytes;
        uint32_t x;

        for (x = 0; x < w; x++)
        {
            uint32_t px = srcrow[col_lut[x]];   /* 0x00RRGGBB */
            uint8_t r = (uint8_t)(px >> 16);
            uint8_t g = (uint8_t)(px >> 8);
            uint8_t b = (uint8_t)(px);
            uint8_t ch[4];

            if (g_fb.pixel_format == 2)
            {
                ch[0] = b; ch[1] = g; ch[2] = r; ch[3] = 0xFF;   /* BGR */
            }
            else
            {
                ch[0] = r; ch[1] = g; ch[2] = b; ch[3] = 0xFF;   /* RGB */
            }

            /* For bpp < 4 the kernel's own pixel writer stores only the
             * first bpp bytes, so mirror that convention. */
            switch (bpp)
            {
            case 4: p[0] = ch[0]; p[1] = ch[1]; p[2] = ch[2]; p[3] = ch[3]; p += 4; break;
            case 3: p[0] = ch[0]; p[1] = ch[1]; p[2] = ch[2]; p += 3; break;
            case 2: p[0] = ch[0]; p[1] = ch[1]; p += 2; break;
            default: p[0] = ch[0]; p += 1; break;
            }
        }
    }

    bedrock_fb_write(0, frame, (size_t)total);

    /* Park once per rendered frame. Without this a long TCG frame starves
     * the kernel audio pump past the HDA ring's staged window and playback
     * underruns. sleep_ms(0) parks this task and lets the idle loop requeue
     * it after any ready work — the pump — has run. (The kernel's tick
     * preemption only fires for ring-3 contexts and only under queue
     * competition, so it must not be relied on here.)
     */
    bedrock_sleep_ms(0);
}

/* ── DG_SleepMs / DG_GetTicksMs ─────────────────────────────────────── */

void DG_SleepMs(uint32_t ms)
{
    bedrock_sleep_ms(ms);
}

uint32_t DG_GetTicksMs(void)
{
    return (uint32_t)(bedrock_now_ns() / 1000000ULL);
}

/* ── DG_GetKey ──────────────────────────────────────────────────────── */

#define KEYQ_CAP 64

typedef struct key_event
{
    unsigned char key;
    unsigned char pressed;
} key_event;

static key_event g_keyq[KEYQ_CAP];
static int       g_keyq_head = 0;
static int       g_keyq_tail = 0;
static uint8_t   g_event_buf[4 + KEYQ_CAP * 24];

static int keyq_full(void)
{
    return ((g_keyq_tail + 1) % KEYQ_CAP) == g_keyq_head;
}

static int keyq_empty(void)
{
    return g_keyq_tail == g_keyq_head;
}

static void keyq_push(unsigned char key, int pressed)
{
    if (keyq_full())
    {
        return;
    }
    g_keyq[g_keyq_tail].key = key;
    g_keyq[g_keyq_tail].pressed = pressed ? 1 : 0;
    g_keyq_tail = (g_keyq_tail + 1) % KEYQ_CAP;
}

static int keyq_pop(unsigned char *key, int *pressed)
{
    if (keyq_empty())
    {
        return 0;
    }
    *key = g_keyq[g_keyq_head].key;
    *pressed = g_keyq[g_keyq_head].pressed;
    g_keyq_head = (g_keyq_head + 1) % KEYQ_CAP;
    return 1;
}

/* Linux KEY_* physical code → Doom KEY_* (doomkeys.h).  Returns 0 for keys
 * Doom has no binding for. */
static unsigned char translate_key(uint32_t code)
{
    switch (code)
    {
    case 1:  return KEY_ESCAPE;
    case 2:  return '1';  case 3:  return '2';  case 4:  return '3';
    case 5:  return '4';  case 6:  return '5';  case 7:  return '6';
    case 8:  return '7';  case 9:  return '8';  case 10: return '9';
    case 11: return '0';
    case 12: return KEY_MINUS;
    case 13: return KEY_EQUALS;
    case 14: return KEY_BACKSPACE;
    case 15: return KEY_TAB;
    case 16: return 'q';  case 17: return 'w';  case 18: return 'e';
    case 19: return 'r';  case 20: return 't';  case 21: return 'y';
    case 22: return 'u';  case 23: return 'i';  case 24: return 'o';
    case 25: return 'p';
    case 26: return '[';
    case 27: return ']';
    case 28: return KEY_ENTER;
    case 29: return KEY_FIRE;       /* left ctrl = fire */
    case 30: return 'a';  case 31: return 's';  case 32: return 'd';
    case 33: return 'f';  case 34: return 'g';  case 35: return 'h';
    case 36: return 'j';  case 37: return 'k';  case 38: return 'l';
    case 39: return ';';
    case 40: return '\'';
    case 41: return '`';
    case 42: return KEY_RSHIFT;     /* left shift */
    case 43: return '\\';
    case 44: return 'z';  case 45: return 'x';  case 46: return 'c';
    case 47: return 'v';  case 48: return 'b';  case 49: return 'n';
    case 50: return 'm';
    case 51: return ',';
    case 52: return '.';
    case 53: return '/';
    case 54: return KEY_RSHIFT;     /* right shift */
    case 55: return '*';
    case 56: return KEY_RALT;       /* left alt = strafe */
    case 57: return KEY_USE;        /* space = use */
    case 58: return KEY_CAPSLOCK;
    case 59: return KEY_F1;   case 60: return KEY_F2;
    case 61: return KEY_F3;   case 62: return KEY_F4;
    case 63: return KEY_F5;   case 64: return KEY_F6;
    case 65: return KEY_F7;   case 66: return KEY_F8;
    case 67: return KEY_F9;   case 68: return KEY_F10;
    case 69: return KEY_NUMLOCK;
    case 70: return KEY_SCRLCK;
    case 71: return KEYP_7;   case 72: return KEYP_8;   case 73: return KEYP_9;
    case 74: return '-';
    case 75: return KEYP_4;   case 76: return '5';      case 77: return KEYP_6;
    case 78: return '+';
    case 79: return KEYP_1;   case 80: return KEYP_2;   case 81: return KEYP_3;
    case 82: return '0';
    case 83: return '.';
    case 87: return KEY_F11;
    case 88: return KEY_F12;
    case 96: return KEY_ENTER;      /* keypad enter */
    case 97: return KEY_FIRE;       /* right ctrl = fire */
    case 98: return '/';
    case 99: return KEY_PRTSCR;
    case 100: return KEY_RALT;      /* right alt = strafe */
    case 102: return KEY_HOME;
    case 103: return KEY_UPARROW;
    case 104: return KEY_PGUP;
    case 105: return KEY_LEFTARROW;
    case 106: return KEY_RIGHTARROW;
    case 107: return KEY_END;
    case 108: return KEY_DOWNARROW;
    case 109: return KEY_PGDN;
    case 110: return KEY_INS;
    case 111: return KEY_DEL;
    case 119: return KEY_PAUSE;
    default:  return 0;
    }
}

static void drain_events(void)
{
    uint32_t count;
    uint32_t avail;
    const uint8_t *p;
    uint32_t i;
    intptr_t n = bedrock_read_events(g_event_buf, sizeof(g_event_buf));

    if (n < 4)
    {
        return;
    }
    count = (uint32_t)g_event_buf[0] | ((uint32_t)g_event_buf[1] << 8) |
            ((uint32_t)g_event_buf[2] << 16) | ((uint32_t)g_event_buf[3] << 24);
    avail = (uint32_t)((n - 4) / 24);
    if (count > KEYQ_CAP)
    {
        count = KEYQ_CAP;
    }
    if (count > avail)
    {
        count = avail;
    }
    p = g_event_buf + 4;
    for (i = 0; i < count; i++)
    {
        /* entry: {timestamp u64, device u32, type u32, code u32, value i32} */
        uint32_t type  = (uint32_t)p[12] | ((uint32_t)p[13] << 8) |
                         ((uint32_t)p[14] << 16) | ((uint32_t)p[15] << 24);
        uint32_t code  = (uint32_t)p[16] | ((uint32_t)p[17] << 8) |
                         ((uint32_t)p[18] << 16) | ((uint32_t)p[19] << 24);
        int32_t  value = (int32_t)((uint32_t)p[20] | ((uint32_t)p[21] << 8) |
                                   ((uint32_t)p[22] << 16) | ((uint32_t)p[23] << 24));
        if (type == 1)              /* InputType::Key */
        {
            unsigned char dk = translate_key(code);
            if (dk != 0)
            {
                keyq_push(dk, value != 0);
            }
        }
        p += 24;
    }
}

int DG_GetKey(int *pressed, unsigned char *key)
{
    if (keyq_pop(key, pressed))
    {
        return 1;
    }
    drain_events();
    if (keyq_pop(key, pressed))
    {
        return 1;
    }
    return 0;
}

/* ── DG_SetWindowTitle ──────────────────────────────────────────────── */

void DG_SetWindowTitle(const char *title)
{
    (void)title;
}

/* ── main ───────────────────────────────────────────────────────────── */

/* DIAGNOSTIC: hardcoded IWAD path — independent of /proc/self/args. */
static char g_argv0[] = "doom";
static char g_argv1[] = "-iwad";
static char g_argv2[] = "/B/EFI/BEDROCK/FREEDOOM.WAD";
static char g_argv3[] = "-mmap";
static char *g_fixed_argv[] = { NULL, NULL, NULL, NULL, NULL };

int main(int argc, char **argv)
{
    (void)argc;
    (void)argv;
    dlog("[doom] main enter\n");
    // chdir already done by Rust entry_main; log args for proof
    dlog("[doom] fixed argv: -iwad /B/EFI/BEDROCK/FREEDOOM.WAD -mmap\n");
    g_fixed_argv[0] = g_argv0;
    g_fixed_argv[1] = g_argv1;
    g_fixed_argv[2] = g_argv2;
    g_fixed_argv[3] = g_argv3;
    dlog("[doom] calling doomgeneric_Create\n");
    doomgeneric_Create(4, g_fixed_argv);
    dlog("[doom] doomgeneric_Create returned\n");

    for (;;)
    {
        if (g_quit)
        {
            bedrock_exit(0);
        }
        doomgeneric_Tick();
    }
}
