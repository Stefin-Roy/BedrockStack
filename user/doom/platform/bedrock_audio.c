/*
 * bedrock_audio.c — shared pull-based audio renderer for the BedrockOS
 * DOOM port.
 *
 * Part of the BedrockOS DOOM port.  The engine is GPL-2.0+ by linkage; this
 * file is our own glue.  It owns the single audio output path used by both
 * the SFX and music modules:
 *
 *   - a wall-clock cursor (bedrock_now_ns) says how many 48 kHz stereo frames
 *     *should* have been played by now;
 *   - each call to bedrock_audio_render() renders up to that cursor — mixing
 *     whatever the SFX and music subsystems contribute — and pushes the result
 *     to the kernel's audio pump via bedrock_audio_play();
 *   - because the cursor is wall-clock based, render() is idempotent: both
 *     the sound module's Update() and the music module's Poll() may call it
 *     in the same tick and exactly one chunk is produced.
 *
 * The kernel pump queues requests and plays them gaplessly at real time, so a
 * full-speed catch-up after a stall only ever costs us a brief cooperative
 * park inside enqueue_playback, never dropped audio.  If the audio device is
 * absent, the first push fails and we go permanently silent.
 */

#include <stdint.h>
#include <stddef.h>

/* Rust bridge (defined in src/main.rs) */
extern intptr_t bedrock_audio_play(const void *src, size_t len);
extern uint64_t bedrock_now_ns(void);

/* Subsystem mix callbacks (bedrock_sfx.c / bedrock_music.c).  Each adds its
 * contribution into an interleaved L/R int32 accumulator. */
void bedrock_sfx_mix(int32_t *buf, int frames);
void bedrock_music_mix(int32_t *buf, int frames);

#define AUDIO_SAMPLE_RATE 48000
/* Stereo frames per push.  4 bytes/frame * 2047 = 8188 B, exactly the
 * payload cap of the Rust scratch buffer (bedrock_audio_play). */
#define AUDIO_CHUNK_FRAMES 2047

static int      g_enabled = 1;
static uint64_t g_start_ns = 0;
static uint64_t g_next_sample = 0;
static int      g_cursor_valid = 0;

static int32_t g_mix[2 * AUDIO_CHUNK_FRAMES];
static int16_t g_out[2 * AUDIO_CHUNK_FRAMES];

/* Note the clock's origin once (at module Init); everything downstream is
 * relative to it. */
void bedrock_audio_start(void)
{
    if (g_start_ns == 0)
    {
        g_start_ns = bedrock_now_ns();
        g_cursor_valid = 0;
    }
}

/* Render whatever is due and push it.  Idempotent within a tick. */
void bedrock_audio_render(void)
{
    uint64_t now, target;
    int i;

    if (!g_enabled || g_start_ns == 0)
    {
        return;
    }

    now = bedrock_now_ns();
    if (now == 0)
    {
        return;
    }

    target = ((now - g_start_ns) * AUDIO_SAMPLE_RATE) / 1000000000ULL;

    /* INIT and WAD loading can take seconds after the audio module starts.
     * Do not synthesize that elapsed wall-clock interval as a burst: it fills
     * the kernel queue, blocks the game behind real-time playback, and delays
     * music until the backlog drains. Start at the first real render point and
     * keep at most one mixer chunk of catch-up after later stalls. */
    if (!g_cursor_valid)
    {
        g_next_sample = target > AUDIO_CHUNK_FRAMES
                      ? target - AUDIO_CHUNK_FRAMES : 0;
        g_cursor_valid = 1;
    }
    else if (target > g_next_sample + (uint64_t)(AUDIO_CHUNK_FRAMES * 2))
    {
        g_next_sample = target - AUDIO_CHUNK_FRAMES;
    }

    while (g_next_sample < target)
    {
        int frames = (int)(target - g_next_sample);
        if (frames > AUDIO_CHUNK_FRAMES)
        {
            frames = AUDIO_CHUNK_FRAMES;
        }

        for (i = 0; i < frames * 2; i++)
        {
            g_mix[i] = 0;
        }
        bedrock_music_mix(g_mix, frames);
        bedrock_sfx_mix(g_mix, frames);

        for (i = 0; i < frames * 2; i++)
        {
            int32_t v = g_mix[i];
            if (v > 32767)
            {
                v = 32767;
            }
            else if (v < -32768)
            {
                v = -32768;
            }
            g_out[i] = (int16_t)v;
        }

        if (bedrock_audio_play(g_out, (size_t)frames * 4) < 0)
        {
            /* No audio device: stay silent, cheaply, for the rest of the run. */
            g_enabled = 0;
            return;
        }
        g_next_sample += (uint64_t)frames;
    }
}
