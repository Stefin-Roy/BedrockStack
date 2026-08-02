//! Audio subsystem smoke test.
//!
//! Plays the startup chime from `B>EFI/Bedrock/startup.wav` (48 kHz / 16-bit /
//! stereo PCM) through the HDA output stream.  The wav is streamed from disk
//! in fixed-size chunks because the HDA DMA staging buffer is capped (256 KiB)
//! and a typical chime exceeds it; the chunk buffer lives on the heap since
//! the kernel stack is only 32 KiB.  When the file is missing or not in the
//! expected format the module falls back to a short ascending C-major melody
//! (C5–E5–G5–C6); without an audio device it skips entirely.

use alloc::vec;
use framebuffer::Framebuffer;

use crate::drivers::serial::SerialPort;
use crate::filesystems::vfs;
use crate::filesystems::vfs::types::OpenFlags;
use crate::filesystems::vfs::types::SeekFrom;
use super::Module;

/// Startup chime location on the ESP (`B>` is the FAT32 system partition).
const WAV_PATH: &str = "B>EFI/Bedrock/startup.wav";
/// One streamed chunk: well under the 256 KiB HDA DMA buffer, and a
/// comfortable heap allocation (the heap starts at 1 MiB and grows on demand).
const CHUNK_BYTES: usize = 32 * 1024;

/// Expected format of the startup chime (spec-asserted by the image).
const WAV_RATE: u32 = 48_000;

/// Outcome of loading the startup chime from disk.
enum WavOutcome {
    /// Played to completion; the payload bytes fed to the codec.
    Played(u64),
    /// File missing or not a 48 kHz / 16-bit / stereo PCM wav.
    Unavailable,
}

pub struct AudioTest;

impl Module for AudioTest {
    fn name(&self) -> &str {
        "audio_test"
    }

    fn version(&self) -> &str {
        "0.1.0"
    }

    fn init(&self, _display: &mut Framebuffer) -> Result<(), &'static str> {
        if !crate::audio::is_ready() {
            SerialPort::puts("[audio] AudioTest SKIP: no audio device\n");
            return Ok(());
        }

        match play_startup_chime() {
            Ok(WavOutcome::Played(bytes)) => {
                let frames = bytes / 4;
                let ms = frames as u64 * 1000 / WAV_RATE as u64;
                SerialPort::puts("[audio] AudioTest PASS: startup chime played (");
                SerialPort::put_u64(bytes);
                SerialPort::puts(" B, ~");
                SerialPort::put_u64(ms);
                SerialPort::puts(" ms)\n");
                Ok(())
            }
            Ok(WavOutcome::Unavailable) => {
                SerialPort::puts("[audio] AudioTest: no usable startup.wav, beeps\n");
                play_melody()
            }
            Err(e) => {
                SerialPort::puts("[audio] AudioTest FAIL: ");
                SerialPort::puts(e);
                SerialPort::puts("\n");
                Err("audio playback failed")
            }
        }
    }
}

/// Play `B>EFI/Bedrock/startup.wav`.  Returns the payload bytes fed to the
/// codec on success; `Unavailable` when the file is absent or not in the
/// expected format; `Err` on a genuine audio-device failure.
fn play_startup_chime() -> Result<WavOutcome, &'static str> {
    let fd = match vfs::open(WAV_PATH, OpenFlags::READ) {
        Ok(fd) => fd,
        Err(_) => return Ok(WavOutcome::Unavailable),
    };
    let result = play_wav_stream(fd);
    let _ = vfs::close(fd);
    result
}

/// Stream a wav file's `data` chunk to the codec.
fn play_wav_stream(fd: u32) -> Result<WavOutcome, &'static str> {
    // RIFF header: "RIFF" + size + "WAVE".
    let mut hdr = [0u8; 12];
    if read_exact(fd, &mut hdr).is_err() || &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        return Ok(WavOutcome::Unavailable);
    }

    // Walk chunks until the `data` chunk; validate the `fmt ` chunk.
    let data_len = loop {
        let mut ch = [0u8; 8];
        if read_exact(fd, &mut ch).is_err() {
            return Ok(WavOutcome::Unavailable);
        }
        let len = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]) as u64;
        if &ch[0..4] == b"fmt " {
            let mut fmt = [0u8; 16];
            if read_exact(fd, &mut fmt).is_err() {
                return Ok(WavOutcome::Unavailable);
            }
            let format = u16::from_le_bytes([fmt[0], fmt[1]]);
            let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            let rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
            if format != 1 || channels != 2 || rate != WAV_RATE || bits != 16 {
                SerialPort::puts("[audio] AudioTest: startup.wav is not 48k/16/stereo PCM (fmt=");
                SerialPort::put_u64(format as u64);
                SerialPort::puts(" ch=");
                SerialPort::put_u64(channels as u64);
                SerialPort::puts(" rate=");
                SerialPort::put_u64(rate as u64);
                SerialPort::puts(" bits=");
                SerialPort::put_u64(bits as u64);
                SerialPort::puts(")\n");
                return Ok(WavOutcome::Unavailable);
            }
            // Skip any extended fmt payload (+ pad byte).
            if len > 16 {
                if skip(fd, len - 16 + (len & 1)).is_err() {
                    return Ok(WavOutcome::Unavailable);
                }
            }
        } else if &ch[0..4] == b"data" {
            break len;
        } else if skip(fd, len + (len & 1)).is_err() {
            return Ok(WavOutcome::Unavailable);
        }
    };

    // Stream the data payload through the HDA ring in chunks.  The chunk
    // buffer is heap-allocated (the BSP stack is only 32 KiB); the driver
    // copies each chunk into its DMA ring as the previous one plays, so
    // playback is gapless and exactly real-time.
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut remaining = data_len;
    let played = crate::audio::play_pcm_stream(
        data_len as u32,
        CHUNK_BYTES,
        &mut || {
            if remaining == 0 {
                return None;
            }
            let want = remaining.min(CHUNK_BYTES as u64) as usize;
            let n = match vfs::read(fd, &mut buf[..want]) {
                Ok(n) => n,
                Err(_) => return None,
            };
            if n == 0 {
                return None; // truncated data chunk
            }
            if n & 3 != 0 {
                return None; // not whole 4-byte frames — corrupt stream
            }
            remaining -= n as u64;
            // buf is heap-aligned (≥ 8 B) and x86_64 is little-endian, so the
            // reinterpretation is aligned and endian-correct.
            let samples = n / 2;
            let pcm: &[i16] =
                unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const i16, samples) };
            Some(pcm.to_vec())
        },
    )?;
    Ok(WavOutcome::Played(played))
}

/// Read until `buf` is filled or the file is exhausted.
fn read_exact(fd: u32, buf: &mut [u8]) -> Result<(), &'static str> {
    let mut off = 0;
    while off < buf.len() {
        let n = vfs::read(fd, &mut buf[off..]).map_err(|_| "wav read failed")?;
        if n == 0 {
            return Err("wav truncated");
        }
        off += n;
    }
    Ok(())
}

/// Skip `len` bytes forward from the current position.
fn skip(fd: u32, len: u64) -> Result<(), &'static str> {
    vfs::seek(fd, SeekFrom::Current(len as i64)).map(|_| ()).map_err(|_| "wav seek failed")
}

/// Fallback: a short ascending C-major melody when no chime is available.
fn play_melody() -> Result<(), &'static str> {
    SerialPort::puts("[audio] AudioTest: playing C5 E5 G5 C6\n");
    let melody: [(u32, u64); 4] = [
        (523, 200),   // C5
        (659, 200),   // E5
        (784, 200),   // G5
        (1046, 300),  // C6
    ];
    for (freq, ms) in melody {
        match crate::audio::play_tone(freq, ms) {
            Ok(()) => {}
            Err(e) => {
                SerialPort::puts("[audio] AudioTest FAIL: ");
                SerialPort::puts(e);
                SerialPort::puts("\n");
                return Err("audio playback failed");
            }
        }
    }
    SerialPort::puts("[audio] AudioTest PASS: melody played\n");
    Ok(())
}
