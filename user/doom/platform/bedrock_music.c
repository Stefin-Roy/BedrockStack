/*
 * bedrock_music.c — music module (DG_music_module) for the BedrockOS DOOM
 * port.
 *
 * Part of the BedrockOS DOOM port.  The engine is GPL-2.0+ by linkage; this
 * file is our own glue.  It converts Doom MUS lumps to PCM:
 *
 *   MUS lump --mus2mid()--> Standard MIDI File (type 0, division 70, no
 *   tempo) --ParseSMF()--> event list; then a light software synth (~24
 *   voices, per-program waveforms + ADSR, percussion on channel 9/10)
 *   renders the song into the shared pull-based renderer (bedrock_audio.c).
 *
 * Doom music has no tempo metadata; mus2mid's output defaults to the classic
 * 140 BPM.  A tempo meta, if one were ever present, is honoured.
 */

#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

#include "i_sound.h"
#include "doomtype.h"
#include "memio.h"
#include "mus2mid.h"

/* Rust bridge + shared renderer */
void bedrock_audio_start(void);
void bedrock_audio_render(void);

#define MUS_OUT_RATE   48000
#define MAX_VOICES     24
#define DEFAULT_TEMPO  428571  /* us per quarter note == 140 BPM */
#define DEFAULT_DIV    70

/* ── parsed song data ───────────────────────────────────────────────── */

typedef struct
{
    uint32_t ticks;   /* absolute tick position */
    uint8_t  type;    /* 0 note on, 1 note off, 2 program, 3 control, 4 tempo */
    uint8_t  chan;
    uint8_t  a;
    uint8_t  b;
    uint32_t val;
} midi_ev_t;

typedef struct midi_song
{
    midi_ev_t *ev;
    int        num;
    int        cap;
    int        division;
    int        tempo_us;
    int        playing;
    int        looping;
    int        pos;          /* event cursor */
    double     tick_pos;
    double     samples_per_tick;
    uint8_t    chan_prog[16];
    uint8_t    chan_vol[16];
    uint8_t    chan_pan[16];
    uint8_t    chan_expr[16];
    uint8_t    sustain[16];
} midi_song_t;

/* ── synthesizer state ──────────────────────────────────────────────── */

typedef struct
{
    int    wave;      /* 0 sine, 1 square, 2 saw, 3 triangle, 4 organ-ish */
    double atk, dec, sus, rel, vol;
} preset_t;

typedef struct
{
    int      active;
    int      chan;
    int      note;
    int      drum;
    int      drum_type;
    uint8_t  sustain_held;
    int      wave;
    uint32_t phase;
    uint32_t step;
    int      env;          /* 0 atk, 1 dec, 2 sus, 3 rel, 4 done */
    double   env_val;
    double   env_inc_atk, env_inc_dec, env_inc_rel;
    double   atk, dec, sus, rel;
    double   vol;
    double   gain_l, gain_r;
} voice_t;

static voice_t    g_voices[MAX_VOICES];
static midi_song_t *g_song = NULL;
static double     g_music_vol = 1.0;

static double     g_freq[128];
static double     g_sine[1024];
static int        g_tables_built = 0;
static uint32_t   g_rng = 0x12345678u;

/* ── tables ─────────────────────────────────────────────────────────── */

static void BuildTables(void)
{
    int i;
    if (g_tables_built)
    {
        return;
    }
    g_tables_built = 1;

    g_freq[69] = 440.0;
    for (i = 69; i < 127; i++)
    {
        g_freq[i + 1] = g_freq[i] * 1.0594630943592953;
    }
    for (i = 69; i > 0; i--)
    {
        g_freq[i - 1] = g_freq[i] / 1.0594630943592953;
    }

    for (i = 0; i < 1024; i++)
    {
        g_sine[i] = sin(6.283185307179586 * (double)i / 1024.0);
    }
}

static double noise_next(void)
{
    g_rng = g_rng * 1664525u + 1013904223u;
    return ((double)(g_rng >> 8) * (2.0 / 16777215.0)) - 1.0;
}

/* ── instrument presets ─────────────────────────────────────────────── */

static preset_t preset_for(int prog)
{
    preset_t p;
    if (prog < 0)
    {
        prog = 0;
    }
    if (prog < 8)       { p = (preset_t){0, 0.002, 0.300, 0.15, 0.250, 0.90}; }
    else if (prog < 16) { p = (preset_t){0, 0.002, 0.200, 0.20, 0.200, 0.90}; }
    else if (prog < 24) { p = (preset_t){4, 0.020, 0.100, 1.00, 0.150, 0.85}; }
    else if (prog < 32) { p = (preset_t){2, 0.002, 0.200, 0.50, 0.100, 0.90}; }
    else if (prog < 40) { p = (preset_t){1, 0.003, 0.100, 0.85, 0.050, 1.00}; }
    else if (prog < 48) { p = (preset_t){0, 0.080, 0.200, 0.90, 0.300, 0.80}; }
    else if (prog < 56) { p = (preset_t){0, 0.050, 0.200, 0.90, 0.250, 0.80}; }
    else if (prog < 64) { p = (preset_t){1, 0.020, 0.100, 0.85, 0.150, 0.90}; }
    else if (prog < 72) { p = (preset_t){3, 0.030, 0.150, 0.90, 0.100, 0.80}; }
    else if (prog < 80) { p = (preset_t){4, 0.020, 0.100, 1.00, 0.150, 0.85}; }
    else if (prog < 88) { p = (preset_t){2, 0.005, 0.100, 0.80, 0.080, 0.90}; }
    else if (prog < 96) { p = (preset_t){0, 0.200, 0.500, 0.90, 0.400, 0.70}; }
    else                { p = (preset_t){2, 0.010, 0.200, 0.60, 0.200, 0.80}; }
    return p;
}

/* ── event list building (Standard MIDI File parser) ────────────────── */

static int PushEvent(midi_song_t *s, uint32_t ticks, uint8_t type,
                     uint8_t chan, uint8_t a, uint8_t b, uint32_t val)
{
    midi_ev_t *e;
    if (s->num >= s->cap)
    {
        int ncap = s->cap ? s->cap * 2 : 256;
        midi_ev_t *ne = (midi_ev_t *)realloc(s->ev, (size_t)ncap * sizeof(midi_ev_t));
        if (ne == NULL)
        {
            return 0;
        }
        s->ev = ne;
        s->cap = ncap;
    }
    e = &s->ev[s->num++];
    e->ticks = ticks;
    e->type = type;
    e->chan = chan;
    e->a = a;
    e->b = b;
    e->val = val;
    return 1;
}

static uint32_t ReadVLQ(const byte **pp, const byte *end, int *ok)
{
    uint32_t v = 0;
    int n = 0;
    for (;;)
    {
        int c;
        if (*pp >= end)
        {
            *ok = 0;
            return 0;
        }
        c = *(*pp)++;
        v = (v << 7) | (uint32_t)(c & 0x7F);
        n++;
        if (n > 4)
        {
            *ok = 0;
            return 0;
        }
        if ((c & 0x80) == 0)
        {
            break;
        }
    }
    return v;
}

static int ReadByte(const byte **pp, const byte *end)
{
    if (*pp >= end)
    {
        return -1;
    }
    return *(*pp)++;
}

static uint32_t ReadBE32(const byte **pp, const byte *end)
{
    uint32_t v = 0;
    int i;
    for (i = 0; i < 4; i++)
    {
        int c = ReadByte(pp, end);
        if (c < 0)
        {
            return 0;
        }
        v = (v << 8) | (uint32_t)c;
    }
    return v;
}

static uint32_t ReadBE16(const byte **pp, const byte *end)
{
    uint32_t v = 0;
    int i;
    for (i = 0; i < 2; i++)
    {
        int c = ReadByte(pp, end);
        if (c < 0)
        {
            return 0;
        }
        v = (v << 8) | (uint32_t)c;
    }
    return v;
}

/* Parse a Standard MIDI File.  Both sources land here: the MUS path converts
 * through mus2mid() (single type-0 track, division 70, no tempo), while
 * freedoom's IWAD already ships its music as MIDI — multi-track type 1, with
 * tempo metas.  Tracks are concatenated onto one timeline: the tick counter
 * keeps accumulating across tracks, matching the file order.  Returns a fully
 * decoded song or NULL on any malformed input — the engine treats a NULL
 * handle as silence. */
static midi_song_t *ParseSMF(const byte *data, size_t len)
{
    const byte *p = data;
    const byte *end = data + len;
    midi_song_t *s;
    uint32_t tick = 0;
    int status = 0;
    int division = 0;
    int tempo_us = 0;
    int ok = 1;

    if (len < 14 || memcmp(p, "MThd", 4) != 0)
    {
        return NULL;
    }
    p += 4;
    if (ReadBE32(&p, end) < 6)
    {
        return NULL;
    }
    (void)ReadBE16(&p, end);    /* format (0 or 1) */
    (void)ReadBE16(&p, end);    /* ntrks */
    division = (int)ReadBE16(&p, end);

    s = (midi_song_t *)calloc(1, sizeof(*s));
    if (s == NULL)
    {
        return NULL;
    }

    while (p < end && ok)
    {
        const byte *trkend;
        uint32_t trklen;

        if (end - p < 8 || memcmp(p, "MTrk", 4) != 0)
        {
            break;   /* not a track chunk: stop, we have what we need */
        }
        p += 4;
        trklen = ReadBE32(&p, end);
        trkend = p + trklen;
        if (trkend > end)
        {
            trkend = end;
        }

        status = 0;   /* running status does not survive a track boundary */

        while (p < trkend && ok)
        {
            uint32_t delta = ReadVLQ(&p, trkend, &ok);
            int ev;
            if (!ok)
            {
                break;
            }
            tick += delta;

            if (p >= trkend)
            {
                break;
            }
            ev = *p++;
            if (ev < 0x80)
            {
                /* running status */
                if (status == 0)
                {
                    ok = 0;
                    break;
                }
                p--;
                ev = status;
            }
            else
            {
                status = ev;
            }

            /* MIDI meta events have the literal status byte 0xFF.  They are not
             * reachable through (ev & 0xF0), whose result is only 0xF0; handle
             * them before dispatching channel/system messages. */
            if (ev == 0xFF)
            {
                int meta = ReadByte(&p, trkend);
                uint32_t mlen = ReadVLQ(&p, trkend, &ok);
                if (meta < 0 || !ok || p + mlen > trkend)
                {
                    ok = 0;
                    break;
                }
                if (meta == 0x51 && mlen >= 3)
                {
                    uint32_t us = ((uint32_t)p[0] << 16) | ((uint32_t)p[1] << 8) | (uint32_t)p[2];
                    if (us == 0)
                    {
                        us = (uint32_t)DEFAULT_TEMPO;
                    }
                    PushEvent(s, tick, 4, 0, 0, 0, us);
                    tempo_us = (int)us;
                }
                p += mlen;
                if (meta == 0x2F)
                {
                    break;   /* end of this track */
                }
                continue;
            }

        switch (ev & 0xF0)
        {
        case 0x80:
        case 0x90:
        {
            int note = ReadByte(&p, trkend);
            int vel = ReadByte(&p, trkend);
            if (note < 0 || vel < 0)
            {
                ok = 0;
                break;
            }
            if ((ev & 0xF0) == 0x80 || vel == 0)
            {
                ok = PushEvent(s, tick, 1, (uint8_t)(ev & 0x0F), (uint8_t)note, 0, 0);
            }
            else
            {
                ok = PushEvent(s, tick, 0, (uint8_t)(ev & 0x0F), (uint8_t)note, (uint8_t)vel, 0);
            }
            break;
        }
        case 0xA0:
        case 0xD0:
        case 0xE0:
            if (ReadByte(&p, trkend) < 0 || ReadByte(&p, trkend) < 0)
            {
                ok = 0;
            }
            break;
        case 0xB0:
        {
            int cc = ReadByte(&p, trkend);
            int val = ReadByte(&p, trkend);
            if (cc < 0 || val < 0)
            {
                ok = 0;
                break;
            }
            ok = PushEvent(s, tick, 3, (uint8_t)(ev & 0x0F), (uint8_t)cc, (uint8_t)val, 0);
            break;
        }
        case 0xC0:
        {
            int prog = ReadByte(&p, trkend);
            if (prog < 0)
            {
                ok = 0;
                break;
            }
            ok = PushEvent(s, tick, 2, (uint8_t)(ev & 0x0F), (uint8_t)prog, 0, 0);
            break;
        }
        case 0xF0:
        {
            uint32_t slen = ReadVLQ(&p, trkend, &ok);
            if (!ok || p + slen > trkend)
            {
                ok = 0;
            }
            else
            {
                p += slen;
            }
            break;
        }
        default:
            ok = 0;
            break;
        }
    }
    }

    if (!ok)
    {
        free(s->ev);
        free(s);
        return NULL;
    }

    if (tempo_us == 0)
    {
        tempo_us = DEFAULT_TEMPO;
    }
    if (division <= 0)
    {
        division = DEFAULT_DIV;
    }

    s->division = division;
    s->tempo_us = tempo_us;
    s->samples_per_tick = ((double)tempo_us / 1000000.0) * (double)MUS_OUT_RATE
                        / (double)division;
    return s;
}

/* ── synthesis ──────────────────────────────────────────────────────── */

static void AllNotesOff(void)
{
    int i;
    for (i = 0; i < MAX_VOICES; i++)
    {
        g_voices[i].active = 0;
        g_voices[i].sustain_held = 0;
    }
}

static int VoiceAlloc(void)
{
    int i;
    for (i = 0; i < MAX_VOICES; i++)
    {
        if (!g_voices[i].active)
        {
            return i;
        }
    }
    /* Pool exhausted: steal the first voice (light synth, acceptable). */
    g_voices[0].active = 0;
    g_voices[0].sustain_held = 0;
    return 0;
}

static void VoiceAdvanceEnv(voice_t *v)
{
    switch (v->env)
    {
    case 0:
        v->env_val += v->env_inc_atk;
        if (v->env_val >= 1.0)
        {
            v->env_val = 1.0;
            v->env = (v->sus <= 0.001) ? 3 : 1;   /* percussive: into release */
        }
        break;
    case 1:
        v->env_val -= v->env_inc_dec;
        if (v->env_val <= v->sus)
        {
            v->env_val = v->sus;
            v->env = 2;
        }
        break;
    case 2:
        break;
    case 3:
        v->env_val -= v->env_inc_rel;
        if (v->env_val <= 0.001)
        {
            v->env_val = 0.0;
            v->env = 4;
        }
        break;
    default:
        break;
    }
}

static double VoiceWave(voice_t *v, uint32_t ph)
{
    switch (v->wave)
    {
    case 0: /* sine */
        return g_sine[(ph >> 22) & 1023];
    case 1: /* square */
        return (ph & 0x80000000u) ? 1.0 : -1.0;
    case 2: /* saw */
        return ((double)(int32_t)ph) * (1.0 / 2147483648.0);
    case 3: /* triangle */
    {
        double x = ((double)(int32_t)ph) * (1.0 / 2147483648.0);
        return x < 0 ? 1.0 + 2.0 * x : 1.0 - 2.0 * x;
    }
    case 4: /* organ-ish: fundamental + 2nd + 3rd harmonic */
        return g_sine[(ph >> 22) & 1023] * 0.5
             + g_sine[((ph << 1) >> 22) & 1023] * 0.3
             + g_sine[((ph + (ph >> 1)) >> 22) & 1023] * 0.2;
    default:
        return 0.0;
    }
}

static double DrumWave(voice_t *v)
{
    switch (v->drum_type)
    {
    case 0: /* bass drum */
        return g_sine[(v->phase >> 22) & 1023];
    case 1: /* snare */
        return noise_next() * 0.8;
    case 2: /* tom */
        return g_sine[(v->phase >> 22) & 1023];
    case 3: /* hi-hat */
        return noise_next() * 0.6;
    case 4: /* cymbal */
        return noise_next() * 0.7;
    default:
        return 0.0;
    }
}

static void StartMelodic(midi_song_t *s, midi_ev_t *e)
{
    preset_t pr = preset_for(s->chan_prog[e->chan]);
    voice_t *v = &g_voices[VoiceAlloc()];
    double pan = (double)s->chan_pan[e->chan];
    uint8_t vel = e->b;

    /* MUS velocities are stored as 0..15; mus2mid writes them through
     * verbatim.  Scale the low nibble up to the full 1..127 range so a MUS
     * lump isn't near-silent (a standard MIDI velocity passes unchanged). */
    if (vel <= 15)
    {
        vel = (uint8_t)((vel * 8) + 7);
    }

    v->active = 1;
    v->chan = e->chan;
    v->note = e->a;
    v->drum = 0;
    v->drum_type = 0;
    v->sustain_held = 0;
    v->wave = pr.wave;
    v->phase = 0;
    v->step = (uint32_t)(g_freq[e->a] * 4294967296.0 / (double)MUS_OUT_RATE);
    v->env = 0;
    v->env_val = 0.0;
    v->atk = pr.atk;
    v->dec = pr.dec;
    v->sus = pr.sus;
    v->rel = pr.rel;
    v->env_inc_atk = pr.atk > 0.0 ? 1.0 / (pr.atk * MUS_OUT_RATE) : 1.0;
    v->env_inc_dec = pr.dec > 0.0 ? (1.0 - pr.sus) / (pr.dec * MUS_OUT_RATE) : 1.0;
    v->env_inc_rel = pr.rel > 0.0 ? 1.0 / (pr.rel * MUS_OUT_RATE) : 1.0;
    v->vol = ((double)s->chan_vol[e->chan] / 127.0) * ((double)vel / 127.0) * pr.vol;
    v->gain_l = pan < 64.0 ? 1.0 : (127.0 - pan) / 63.0;
    v->gain_r = pan > 64.0 ? 1.0 : pan / 64.0;
}

static void StartDrum(midi_song_t *s, midi_ev_t *e)
{
    voice_t *v = &g_voices[VoiceAlloc()];
    double decay;
    double freq;
    uint8_t vel = e->b;

    if (vel <= 15)
    {
        vel = (uint8_t)((vel * 8) + 7);
    }

    (void)s;
    v->active = 1;
    v->chan = e->chan;
    v->note = e->a;
    v->drum = 1;
    v->sustain_held = 0;
    v->phase = 0;
    v->env = 0;
    v->env_val = 0.0;
    v->atk = 0.001;
    v->dec = 0.0;
    v->sus = 0.0;
    v->gain_l = 1.0;
    v->gain_r = 1.0;
    v->vol = ((double)vel / 127.0) * 0.9;

    switch (e->a)
    {
    case 35: case 36:           /* bass drum */
        v->drum_type = 0;
        freq = 50.0;
        decay = 0.40;
        break;
    case 38: case 40:           /* snare */
        v->drum_type = 1;
        freq = 0.0;
        decay = 0.18;
        break;
    case 41: case 43: case 45: case 47: case 48: case 50:  /* toms */
        v->drum_type = 2;
        freq = g_freq[e->a];
        decay = 0.30;
        break;
    case 42: case 44: case 46:  /* hi-hat */
        v->drum_type = 3;
        freq = 0.0;
        decay = 0.08;
        break;
    default:                    /* cymbals etc. */
        v->drum_type = 4;
        freq = 0.0;
        decay = 0.80;
        break;
    }

    if (v->drum_type == 0 || v->drum_type == 2)
    {
        v->step = (uint32_t)(freq * 4294967296.0 / (double)MUS_OUT_RATE);
    }
    else
    {
        v->step = 0;
    }

    v->rel = decay;
    v->env_inc_atk = 1.0 / (0.001 * MUS_OUT_RATE);
    v->env_inc_dec = 1.0;
    v->env_inc_rel = decay > 0.0 ? 1.0 / (decay * MUS_OUT_RATE) : 1.0;
}

static void ReleaseNote(uint8_t chan, uint8_t note)
{
    int i;
    for (i = 0; i < MAX_VOICES; i++)
    {
        voice_t *v = &g_voices[i];
        if (!v->active || v->drum || v->chan != chan || v->note != note)
        {
            continue;
        }
        if (g_song != NULL && g_song->sustain[chan])
        {
            v->sustain_held = 1;   /* pedal holds it; released when pedal lifts */
        }
        else if (v->env != 3)
        {
            v->env = 3;
        }
    }
}

static void ReleaseChannel(uint8_t chan)
{
    int i;
    for (i = 0; i < MAX_VOICES; i++)
    {
        voice_t *v = &g_voices[i];
        if (v->active && v->chan == chan && v->env != 3)
        {
            v->env = 3;
        }
    }
}

static void ReleaseSustained(uint8_t chan)
{
    int i;
    for (i = 0; i < MAX_VOICES; i++)
    {
        voice_t *v = &g_voices[i];
        if (v->active && v->sustain_held && v->chan == chan)
        {
            v->sustain_held = 0;
            v->env = 3;
        }
    }
}

/* ── event firing ───────────────────────────────────────────────────── */

static void FireEvent(midi_song_t *s, midi_ev_t *e)
{
    switch (e->type)
    {
    case 0:   /* note on */
        if (e->b == 0)
        {
            ReleaseNote(e->chan, e->a);
        }
        else if (e->chan == 9)
        {
            StartDrum(s, e);
        }
        else
        {
            StartMelodic(s, e);
        }
        break;
    case 1:   /* note off */
        ReleaseNote(e->chan, e->a);
        break;
    case 2:   /* program change */
        s->chan_prog[e->chan] = e->a;
        break;
    case 3:   /* control change */
        switch (e->a)
        {
        case 7:                            /* volume */
            s->chan_vol[e->chan] = e->b;
            break;
        case 10:                           /* pan */
            s->chan_pan[e->chan] = e->b;
            break;
        case 11:                           /* expression */
            s->chan_expr[e->chan] = e->b;
            break;
        case 64:                           /* sustain pedal */
            if (e->b < 64)
            {
                s->sustain[e->chan] = 0;
                ReleaseSustained(e->chan);
            }
            else
            {
                s->sustain[e->chan] = 1;
            }
            break;
        case 0x78: case 0x79: case 0x7B:   /* all sound/notes off */
            ReleaseChannel(e->chan);
            break;
        default:
            break;
        }
        break;
    case 4:   /* tempo */
        if (e->val != 0)
        {
            s->tempo_us = (int)e->val;
            s->samples_per_tick = ((double)s->tempo_us / 1000000.0)
                                * (double)MUS_OUT_RATE / (double)s->division;
        }
        break;
    default:
        break;
    }
}

/* ── mixing into the shared renderer ────────────────────────────────── */

void bedrock_music_mix(int32_t *buf, int frames)
{
    midi_song_t *s = g_song;
    int i;

    if (s == NULL || !s->playing || s->num == 0)
    {
        return;
    }

    for (i = 0; i < frames; i++)
    {
        int v;

        s->tick_pos += 1.0 / s->samples_per_tick;
        while (s->pos < s->num && (double)s->ev[s->pos].ticks <= s->tick_pos)
        {
            FireEvent(s, &s->ev[s->pos]);
            s->pos++;
        }
        if (s->pos >= s->num)
        {
            if (s->looping)
            {
                s->pos = 0;
                s->tick_pos = 0.0;
                AllNotesOff();
            }
            else
            {
                s->playing = 0;
                AllNotesOff();
                return;
            }
        }

        for (v = 0; v < MAX_VOICES; v++)
        {
            voice_t *vo = &g_voices[v];
            double sval;
            if (!vo->active)
            {
                continue;
            }
            VoiceAdvanceEnv(vo);
            if (vo->env == 4)
            {
                vo->active = 0;
                continue;
            }
            vo->phase += vo->step;
            sval = (vo->drum ? DrumWave(vo) : VoiceWave(vo, vo->phase))
                 * vo->env_val * vo->vol * g_music_vol;
            buf[i * 2 + 0] += (int32_t)(sval * vo->gain_l * 16384.0);
            buf[i * 2 + 1] += (int32_t)(sval * vo->gain_r * 16384.0);
            if (vo->drum && vo->drum_type == 0)
            {
                vo->step = (uint32_t)((double)vo->step * 0.9985);
            }
        }
    }
}

/* ── module entry points ────────────────────────────────────────────── */

static boolean I_Bedrock_InitMusic(void)
{
    BuildTables();
    g_music_vol = 1.0;
    g_song = NULL;
    AllNotesOff();
    g_rng = 0x12345678u;
    bedrock_audio_start();
    return true;
}

static void I_Bedrock_ShutdownMusic(void)
{
    AllNotesOff();
    g_song = NULL;
}

static void I_Bedrock_SetMusicVolume(int volume)
{
    if (volume < 0)
    {
        volume = 0;
    }
    else if (volume > 127)
    {
        volume = 127;
    }
    /* Linear-ish map with a gentle boost so default (64) sits ~0.85. */
    g_music_vol = (double)volume / 75.0;
    if (g_music_vol > 1.0)
    {
        g_music_vol = 1.0;
    }
}

static void I_Bedrock_PauseMusic(void)
{
    if (g_song != NULL)
    {
        g_song->playing = 0;
    }
}

static void I_Bedrock_ResumeMusic(void)
{
    if (g_song != NULL)
    {
        g_song->playing = 1;
    }
}

static void *I_Bedrock_RegisterSong(void *data, int len)
{
    MEMFILE *in = NULL;
    MEMFILE *out = NULL;
    void *midbuf = NULL;
    size_t midlen = 0;
    midi_song_t *s = NULL;

    if (data == NULL || len <= 0)
    {
        return NULL;
    }

    if (len >= 4 && memcmp(data, "MThd", 4) == 0)
    {
        /* Already a MIDI file — freedoom ships its music lumps as MIDI. */
        return ParseSMF((const byte *)data, (size_t)len);
    }

    in = mem_fopen_read(data, (size_t)len);
    out = mem_fopen_write();
    if (in == NULL || out == NULL)
    {
        goto done;
    }
    if (mus2mid(in, out))
    {
        goto done;
    }
    mem_get_buf(out, &midbuf, &midlen);
    s = ParseSMF((const byte *)midbuf, midlen);

done:
    if (in != NULL)
    {
        mem_fclose(in);
    }
    if (out != NULL)
    {
        mem_fclose(out);
    }
    return s;
}

static void I_Bedrock_UnRegisterSong(void *handle)
{
    midi_song_t *s = (midi_song_t *)handle;
    if (s == NULL)
    {
        return;
    }
    if (g_song == s)
    {
        g_song = NULL;
    }
    free(s->ev);
    free(s);
}

static void I_Bedrock_PlaySong(void *handle, boolean looping)
{
    midi_song_t *s = (midi_song_t *)handle;
    int i;
    if (s == NULL)
    {
        return;
    }
    g_song = s;
    s->playing = 1;
    s->looping = looping ? 1 : 0;
    s->pos = 0;
    s->tick_pos = 0.0;
    for (i = 0; i < 16; i++)
    {
        s->chan_prog[i] = 0;
        s->chan_vol[i] = 100;
        s->chan_pan[i] = 64;
        s->chan_expr[i] = 127;
        s->sustain[i] = 0;
    }
    AllNotesOff();
}

static void I_Bedrock_StopSong(void)
{
    if (g_song != NULL)
    {
        g_song->playing = 0;
    }
    g_song = NULL;
    AllNotesOff();
}

static boolean I_Bedrock_MusicIsPlaying(void)
{
    return g_song != NULL && g_song->playing;
}

static void I_Bedrock_PollMusic(void)
{
    /* Pull-based renderer: this (like the sound module's Update) drives it;
     * calling it from both is safe because it is idempotent per tick. */
    bedrock_audio_render();
}

/* ── module ─────────────────────────────────────────────────────────── */

static snddevice_t music_devices[] =
{
    SNDDEVICE_SB,
    SNDDEVICE_ADLIB,
    SNDDEVICE_PAS,
    SNDDEVICE_GUS,
    SNDDEVICE_WAVEBLASTER,
    SNDDEVICE_SOUNDCANVAS,
    SNDDEVICE_GENMIDI,
    SNDDEVICE_AWE32,
};

music_module_t DG_music_module =
{
    music_devices,
    (int)(sizeof(music_devices) / sizeof(music_devices[0])),
    I_Bedrock_InitMusic,
    I_Bedrock_ShutdownMusic,
    I_Bedrock_SetMusicVolume,
    I_Bedrock_PauseMusic,
    I_Bedrock_ResumeMusic,
    I_Bedrock_RegisterSong,
    I_Bedrock_UnRegisterSong,
    I_Bedrock_PlaySong,
    I_Bedrock_StopSong,
    I_Bedrock_MusicIsPlaying,
    I_Bedrock_PollMusic,
};
