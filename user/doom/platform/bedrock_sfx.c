/*
 * bedrock_sfx.c — sound-effect module (DG_sound_module) for the BedrockOS
 * DOOM port.
 *
 * Part of the BedrockOS DOOM port.  The engine is GPL-2.0+ by linkage; this
 * file is our own glue.  It decodes Doom's native DMX sound lumps (8-bit
 * unsigned PCM) and mixes up to 16 simultaneous channels into the shared
 * pull-based renderer (bedrock_audio.c).
 *
 * Lump format (identical to chocolate-doom's i_sdlsound.c CacheSFX):
 *   bytes 0-1    0x03 0x00          8-bit format marker
 *   bytes 2-3    little-endian u16  sample rate (usually 11025 or 22050)
 *   bytes 4-7    little-endian u32  sample length
 *   bytes 8-23   DMX header + padding (skipped, like DMX itself does)
 *   bytes 24..   the 8-bit unsigned PCM payload
 * The last 16 bytes of the declared length are also discarded, matching DMX.
 */

#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include "i_sound.h"
#include "w_wad.h"
#include "z_zone.h"
#include "doomtype.h"

/* Rust bridge + shared renderer */
extern intptr_t bedrock_audio_play(const void *src, size_t len);
void bedrock_audio_start(void);
void bedrock_audio_render(void);

#define SFX_OUT_RATE 48000
#define SFX_CHANNELS 16

/* A decoded, cache-resident sound: mono i16 at SFX_OUT_RATE. */
typedef struct
{
    int16_t *samples;
    int      len;   /* number of output frames */
} bedrock_sfx_data_t;

typedef struct
{
    bedrock_sfx_data_t *snd;
    unsigned long       pos;      /* 16.16 fixed point into snd->samples */
    int                 done;
    int32_t             gain_l;   /* sample * gain >> 15 */
    int32_t             gain_r;
} sfx_channel_t;

static sfx_channel_t g_channels[SFX_CHANNELS];
static boolean       g_sfx_active = false;

/* Config bindings referenced by i_sound.c's I_BindSoundVariables; our
 * decoder always runs at native rate, so libsamplerate stays disabled. */
int   use_libsamplerate = 0;
float libsamplerate_scale = 0.65f;

/* ── lump decoding ──────────────────────────────────────────────────── */

static bedrock_sfx_data_t *DecodeSfx(sfxinfo_t *sfxinfo)
{
    int lumpnum = sfxinfo->lumpnum;
    unsigned int lumplen;
    int samplerate;
    unsigned int length;
    unsigned int i;
    unsigned long pos, step;
    byte *data;
    bedrock_sfx_data_t *snd;
    unsigned int outlen;

    if (lumpnum < 0)
    {
        return NULL;
    }

    data = (byte *)W_CacheLumpNum(lumpnum, PU_STATIC);
    lumplen = W_LumpLength(lumpnum);

    if (lumplen < 8 || data[0] != 0x03 || data[1] != 0x00)
    {
        W_ReleaseLumpNum(lumpnum);
        return NULL;
    }

    samplerate = (data[3] << 8) | data[2];
    length = ((unsigned int)data[7] << 24) | ((unsigned int)data[6] << 16)
           | ((unsigned int)data[5] << 8) | (unsigned int)data[4];

    if (length > lumplen - 8 || length <= 48)
    {
        W_ReleaseLumpNum(lumpnum);
        return NULL;
    }

    /* Skip the 16-byte DMX prefix and its 8-byte per-sound header; the last
     * 16 bytes of the declared length are discarded as DMX does. */
    data += 24;
    length -= 32;

    if (samplerate <= 0)
    {
        W_ReleaseLumpNum(lumpnum);
        return NULL;
    }

    outlen = (unsigned int)((((uint64_t)length * SFX_OUT_RATE) / (uint64_t)samplerate) + 1);

    snd = (bedrock_sfx_data_t *)malloc(sizeof(*snd));
    if (snd == NULL)
    {
        W_ReleaseLumpNum(lumpnum);
        return NULL;
    }
    snd->samples = (int16_t *)malloc(outlen * sizeof(int16_t));
    if (snd->samples == NULL)
    {
        free(snd);
        W_ReleaseLumpNum(lumpnum);
        return NULL;
    }
    snd->len = (int)outlen;

    /* Resample 8-bit unsigned mono at samplerate -> i16 mono at 48 kHz,
     * nearest-neighbour stepping. */
    step = (unsigned long)(((uint64_t)samplerate << 16) / SFX_OUT_RATE);
    pos = 0;
    for (i = 0; i < outlen; i++)
    {
        unsigned long src = pos >> 16;
        int16_t sample = 0;
        if (src < length)
        {
            sample = (int16_t)(((int)data[src] - 128) << 8);
        }
        snd->samples[i] = sample;
        pos += step;
    }

    W_ReleaseLumpNum(lumpnum);
    return snd;
}

/* ── module entry points ────────────────────────────────────────────── */

static boolean I_Bedrock_InitSound(boolean use_sfx_prefix)
{
    (void)use_sfx_prefix;
    memset(g_channels, 0, sizeof(g_channels));
    g_sfx_active = true;
    bedrock_audio_start();
    return true;
}

static void I_Bedrock_ShutdownSound(void)
{
    g_sfx_active = false;
}

static int I_Bedrock_GetSfxLumpNum(sfxinfo_t *sfx)
{
    char namebuf[16];
    if (sfx->link != NULL)
    {
        sfx = sfx->link;
    }
    /* Doom prefixes all sound lumps with "ds". */
    if (snprintf(namebuf, sizeof(namebuf), "ds%s", sfx->name) <= 0)
    {
        return -1;
    }
    return W_CheckNumForName(namebuf);
}

static void I_Bedrock_UpdateSoundParams(int channel, int vol, int sep)
{
    int left, right;
    if (!g_sfx_active || channel < 0 || channel >= SFX_CHANNELS)
    {
        return;
    }
    left = ((254 - sep) * vol) / 127;
    right = (sep * vol) / 127;
    if (left < 0)      { left = 0; }
    else if (left > 254){ left = 254; }
    if (right < 0)      { right = 0; }
    else if (right > 254){ right = 254; }

    /* Scale 0..254 -> 0..16384 (half-scale headroom for the mix). */
    g_channels[channel].gain_l = (int32_t)(((int64_t)left * 16384) / 254);
    g_channels[channel].gain_r = (int32_t)(((int64_t)right * 16384) / 254);
}

static int I_Bedrock_StartSound(sfxinfo_t *sfxinfo, int channel, int vol, int sep)
{
    if (!g_sfx_active || channel < 0 || channel >= SFX_CHANNELS)
    {
        return -1;
    }

    g_channels[channel].done = 1;   /* kill whatever was playing */

    if (sfxinfo->driver_data == NULL)
    {
        sfxinfo->driver_data = DecodeSfx(sfxinfo);
    }
    if (sfxinfo->driver_data == NULL)
    {
        return -1;
    }

    g_channels[channel].snd = (bedrock_sfx_data_t *)sfxinfo->driver_data;
    g_channels[channel].pos = 0;
    g_channels[channel].done = 0;
    I_Bedrock_UpdateSoundParams(channel, vol, sep);
    return channel;
}

static void I_Bedrock_StopSound(int channel)
{
    if (!g_sfx_active || channel < 0 || channel >= SFX_CHANNELS)
    {
        return;
    }
    g_channels[channel].done = 1;
}

static boolean I_Bedrock_SoundIsPlaying(int channel)
{
    if (!g_sfx_active || channel < 0 || channel >= SFX_CHANNELS)
    {
        return false;
    }
    return g_channels[channel].snd != NULL && !g_channels[channel].done;
}

static void I_Bedrock_UpdateSound(void)
{
    int i;

    if (!g_sfx_active)
    {
        return;
    }

    for (i = 0; i < SFX_CHANNELS; i++)
    {
        if (g_channels[i].snd != NULL && !g_channels[i].done
            && (g_channels[i].pos >> 16) >= (unsigned long)g_channels[i].snd->len)
        {
            g_channels[i].done = 1;
        }
    }

    /* Drive the shared pull renderer (also invoked by the music module). */
    bedrock_audio_render();
}

static void I_Bedrock_CacheSounds(sfxinfo_t *sounds, int num_sounds)
{
    (void)sounds;
    (void)num_sounds;
    /* Decode lazily in StartSound. */
}

/* ── mixing into the shared renderer ────────────────────────────────── */

void bedrock_sfx_mix(int32_t *buf, int frames)
{
    int c;

    if (!g_sfx_active)
    {
        return;
    }

    for (c = 0; c < SFX_CHANNELS; c++)
    {
        sfx_channel_t *ch = &g_channels[c];
        int i;

        if (ch->done || ch->snd == NULL)
        {
            continue;
        }

        for (i = 0; i < frames; i++)
        {
            unsigned long src;
            int32_t s;

            if ((ch->pos >> 16) >= (unsigned long)ch->snd->len)
            {
                ch->done = 1;
                break;
            }
            src = ch->pos >> 16;
            s = ch->snd->samples[src];
            buf[i * 2 + 0] += (s * ch->gain_l) >> 15;
            buf[i * 2 + 1] += (s * ch->gain_r) >> 15;
            ch->pos += 0x10000;   /* source is already 48 kHz: 1 frame per step */
        }
    }
}

/* ── module ─────────────────────────────────────────────────────────── */

static snddevice_t sfx_devices[] =
{
    SNDDEVICE_SB,
    SNDDEVICE_PAS,
    SNDDEVICE_GUS,
    SNDDEVICE_WAVEBLASTER,
    SNDDEVICE_SOUNDCANVAS,
    SNDDEVICE_AWE32,
    SNDDEVICE_PCSPEAKER,
    SNDDEVICE_ADLIB,
};

sound_module_t DG_sound_module =
{
    sfx_devices,
    (int)(sizeof(sfx_devices) / sizeof(sfx_devices[0])),
    I_Bedrock_InitSound,
    I_Bedrock_ShutdownSound,
    I_Bedrock_GetSfxLumpNum,
    I_Bedrock_UpdateSound,
    I_Bedrock_UpdateSoundParams,
    I_Bedrock_StartSound,
    I_Bedrock_StopSound,
    I_Bedrock_SoundIsPlaying,
    I_Bedrock_CacheSounds,
};