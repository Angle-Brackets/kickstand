// SPDX-License-Identifier: GPL-3.0-only
//! Console-side 5-second circular PCM audio ring buffer ("flight recorder") and live chunk streaming.
//!
//! At 48 kHz 16-bit stereo (`192,000` bytes/s), 5 seconds of uncompressed PCM is `960,000` bytes (~937.5 KiB).
//! The ring buffer is allocated lazily on first use with `try_reserve_exact` and read in place by chunk
//! offset so `audio_replay` never duplicates the 960 KiB buffer in a client session's heap.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use nx::svc;
use proto::{ErrorKind, MAX_CHUNK, Response};
use services::audio::{
    AudioStatus, BYTES_PER_FRAME, BYTES_PER_SAMPLE, BYTES_PER_SECOND, CHANNELS, SAMPLE_RATE,
};

use crate::uptime_ms;

/// Maximum duration held in the retroactive circular buffer (5 seconds = 960,000 bytes).
pub const MAX_RING_MS: u32 = 5_000;
/// Stereo `i16` samples per second (`48,000 * 2 = 96,000`).
pub const SAMPLES_PER_SECOND: usize = (SAMPLE_RATE as usize) * (CHANNELS as usize);
/// Total `i16` samples in the 5-second circular buffer (`480,000` samples = `960,000` bytes).
pub const RING_CAPACITY_SAMPLES: usize = SAMPLES_PER_SECOND * (MAX_RING_MS as usize / 1000);
/// Default proactive streaming chunk duration in ms (`160` ms = `30,720` bytes, ~30 KiB).
pub const DEFAULT_STREAM_CHUNK_MS: u32 = 160;
/// Maximum single proactive streaming chunk duration in ms (`320` ms = `61,440` bytes, under `MAX_CHUNK`).
pub const MAX_STREAM_CHUNK_MS: u32 = 320;

static SAMPLER_RUNNING: AtomicBool = AtomicBool::new(false);
static RING: nx::sync::Mutex<RingBuffer> = nx::sync::Mutex::new(RingBuffer::new());

/// In-memory circular buffer of interleaved stereo `i16` PCM samples at 48 kHz.
pub struct RingBuffer {
    /// Lazily allocated backing storage of length `RING_CAPACITY_SAMPLES`.
    samples: Vec<i16>,
    /// Next write index in `0..RING_CAPACITY_SAMPLES` (always even for stereo frame alignment).
    write_pos: usize,
    /// Number of valid `i16` samples currently stored (`0..=RING_CAPACITY_SAMPLES`, always even).
    filled: usize,
    /// Total `i16` samples ever pushed since boot.
    total_written: u64,
    /// Uptime in ms when samples were last pushed.
    last_push_ms: u64,
    /// Frozen replay window length in bytes set by [`prepare_replay`], read in chunks by [`read_replay_chunk`].
    replay_bytes: usize,
    /// Write position snapshot when [`prepare_replay`] was called.
    replay_write_pos: usize,
    /// Filled sample count snapshot when [`prepare_replay`] was called.
    replay_filled: usize,
}

impl RingBuffer {
    pub const fn new() -> Self {
        Self {
            samples: Vec::new(),
            write_pos: 0,
            filled: 0,
            total_written: 0,
            last_push_ms: 0,
            replay_bytes: 0,
            replay_write_pos: 0,
            replay_filled: 0,
        }
    }

    /// Lazily allocates the 960 KiB ring buffer on first use without panicking on OOM.
    fn ensure_allocated(&mut self) -> bool {
        if self.samples.len() == RING_CAPACITY_SAMPLES {
            return true;
        }
        if self.samples.try_reserve_exact(RING_CAPACITY_SAMPLES).is_err() {
            return false;
        }
        self.samples.resize(RING_CAPACITY_SAMPLES, 0);
        true
    }

    /// Pushes interleaved stereo `i16` samples into the circular buffer.
    pub fn push_samples(&mut self, pcm: &[i16], now_ms: u64) -> bool {
        let aligned_len = pcm.len() & !1;
        if aligned_len == 0 {
            return true;
        }
        if !self.ensure_allocated() {
            return false;
        }
        let slice = &pcm[..aligned_len];
        let src = if slice.len() > RING_CAPACITY_SAMPLES {
            &slice[slice.len() - RING_CAPACITY_SAMPLES..]
        } else {
            slice
        };
        let first_part = (RING_CAPACITY_SAMPLES - self.write_pos).min(src.len());
        self.samples[self.write_pos..self.write_pos + first_part]
            .copy_from_slice(&src[..first_part]);
        let second_part = src.len() - first_part;
        if second_part > 0 {
            self.samples[..second_part].copy_from_slice(&src[first_part..]);
        }
        self.write_pos = (self.write_pos + src.len()) % RING_CAPACITY_SAMPLES;
        self.filled = (self.filled + src.len()).min(RING_CAPACITY_SAMPLES);
        self.total_written = self.total_written.saturating_add(src.len() as u64);
        self.last_push_ms = now_ms;
        true
    }

    /// Pushes little-endian `i16` stereo bytes into the circular buffer.
    pub fn push_bytes(&mut self, pcm_le: &[u8], now_ms: u64) -> bool {
        let frame_bytes = pcm_le.len() & !(BYTES_PER_FRAME - 1);
        if frame_bytes == 0 {
            return true;
        }
        if !self.ensure_allocated() {
            return false;
        }
        for chunk in pcm_le[..frame_bytes].chunks_exact(2) {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            self.samples[self.write_pos] = sample;
            self.write_pos += 1;
            if self.write_pos == RING_CAPACITY_SAMPLES {
                self.write_pos = 0;
            }
        }
        let added_samples = frame_bytes / BYTES_PER_SAMPLE;
        self.filled = (self.filled + added_samples).min(RING_CAPACITY_SAMPLES);
        self.total_written = self.total_written.saturating_add(added_samples as u64);
        self.last_push_ms = now_ms;
        true
    }

    /// Snapshots the ring buffer cursor for a retroactive replay of `duration_ms` (clamped to `1..=5000` ms)
    /// and returns the total byte length of the replay window.
    pub fn prepare_replay(&mut self, duration_ms: u32) -> usize {
        let ms = duration_ms.clamp(1, MAX_RING_MS) as usize;
        let wanted_frames = (ms * (SAMPLE_RATE as usize)) / 1000;
        let wanted_samples = (wanted_frames * (CHANNELS as usize)).min(RING_CAPACITY_SAMPLES);
        self.replay_bytes = wanted_samples * BYTES_PER_SAMPLE;
        self.replay_write_pos = self.write_pos;
        self.replay_filled = self.filled;
        self.replay_bytes
    }

    /// Reads `[offset .. offset + len]` bytes of little-endian `i16` PCM from the prepared replay window
    /// without allocating a second copy of the 5-second buffer.
    ///
    /// Any leading portion before the earliest sample in the ring buffer is zero-filled (silence).
    pub fn read_replay_chunk(&self, offset: u64, len: u32) -> (Vec<u8>, u64) {
        let total = self.replay_bytes;
        let start_byte = (offset as usize).min(total) & !(BYTES_PER_SAMPLE - 1);
        let max_len = (len as usize).min(MAX_CHUNK) & !(BYTES_PER_SAMPLE - 1);
        let end_byte = start_byte.saturating_add(max_len).min(total);
        if start_byte >= end_byte {
            return (Vec::new(), total as u64);
        }

        let wanted_samples = total / BYTES_PER_SAMPLE;
        let available_samples = self.replay_filled.min(wanted_samples);
        let silence_prefix_samples = wanted_samples.saturating_sub(available_samples);
        let ring_start_sample = if self.samples.len() == RING_CAPACITY_SAMPLES {
            (self.replay_write_pos + RING_CAPACITY_SAMPLES - available_samples) % RING_CAPACITY_SAMPLES
        } else {
            0
        };

        let start_sample = start_byte / BYTES_PER_SAMPLE;
        let end_sample = end_byte / BYTES_PER_SAMPLE;
        let mut out = Vec::with_capacity((end_sample - start_sample) * BYTES_PER_SAMPLE);

        for sample_idx in start_sample..end_sample {
            let sample = if sample_idx < silence_prefix_samples || self.samples.is_empty() {
                0i16
            } else {
                let ring_offset = sample_idx - silence_prefix_samples;
                let pos = (ring_start_sample + ring_offset) % RING_CAPACITY_SAMPLES;
                self.samples[pos]
            };
            out.extend_from_slice(&sample.to_le_bytes());
        }
        (out, total as u64)
    }
}

/// Marks the audio ring buffer subsystem ready without spawning a background `audin:u` polling thread.
pub fn start_sampler() {
    SAMPLER_RUNNING.store(true, Ordering::SeqCst);
}

/// Pushes raw PCM samples directly into the global 5-second ring buffer.
pub fn push_samples(pcm: &[i16]) -> bool {
    RING.lock().push_samples(pcm, uptime_ms())
}

/// Pushes raw little-endian `i16` PCM bytes directly into the global 5-second ring buffer.
pub fn push_bytes(pcm_le: &[u8]) -> bool {
    RING.lock().push_bytes(pcm_le, uptime_ms())
}

/// Reads and resamples 32,728 Hz stereo `i16` frames from a 32,768-frame circular buffer (`g_ring`)
/// in a debugged process into 48,000 Hz stereo `i16` samples in `dst`.
fn resample_ring_32k_to_48k(
    dbg: &services::debug::Debugged,
    ring_base: u64,
    w: u64,
    dst: &mut [i16],
) -> usize {
    let out_frames = (dst.len() / 2).min(48_000);
    if out_frames == 0 {
        return 0;
    }
    let in_frames = ((out_frames * 32_728) / 48_000).clamp(1, 32_768);
    let start_frame = w.saturating_sub(in_frames as u64);
    let mut block = [0u8; 4096];
    let mut loaded_blk = usize::MAX;
    for out_idx in 0..out_frames {
        let src_off = (out_idx * 32_728) / 48_000;
        let ring_frame = ((start_frame + src_off as u64) as usize) & 32_767;
        let blk = ring_frame >> 10;
        if blk != loaded_blk {
            let addr = ring_base.wrapping_add((blk as u64) * 4096);
            if dbg.read(addr, &mut block).is_err() {
                return out_idx * 2;
            }
            loaded_blk = blk;
        }
        let byte_off = (ring_frame & 1023) * 4;
        dst[out_idx * 2] = i16::from_le_bytes([block[byte_off], block[byte_off + 1]]);
        dst[out_idx * 2 + 1] = i16::from_le_bytes([block[byte_off + 2], block[byte_off + 3]]);
    }
    out_frames * 2
}

/// Reads linear little-endian `i16` stereo samples from a process's `AudioOutBuffer` data pointer.
fn read_linear_pcm(
    dbg: &services::debug::Debugged,
    buf_ptr: u64,
    data_sz: usize,
    dst: &mut [i16],
) -> usize {
    let max_samples = (data_sz / 2).min(dst.len()) & !1;
    if max_samples == 0 {
        return 0;
    }
    let mut block = [0u8; 4096];
    let mut written = 0usize;
    while written < max_samples {
        let chunk_samples = (max_samples - written).min(block.len() / 2);
        let chunk_bytes = chunk_samples * 2;
        let addr = buf_ptr.wrapping_add((written * 2) as u64);
        if dbg.read(addr, &mut block[..chunk_bytes]).is_err() {
            break;
        }
        for i in 0..chunk_samples {
            dst[written + i] = i16::from_le_bytes([block[i * 2], block[i * 2 + 1]]);
        }
        written += chunk_samples;
    }
    written & !1
}

/// Captures live PCM stereo samples from the currently running application's audio buffers via
/// `svcDebugActiveProcess` / `svcReadDebugProcessMemory` without opening `audout:u` or `audin:u` handles.
fn capture_from_running_app(dst: &mut [i16]) -> usize {
    let Ok(Some(app)) = services::pm::application() else {
        return 0;
    };
    let pid = app.pid;
    let Ok(dbg) = services::debug::Debugged::attach(pid) else {
        return 0;
    };
    if dbg.pause().is_err() {
        return 0;
    }

    // Fast path: check executable NRO mappings for the 32,768-frame stereo ring (`g_ring` / `g_write`).
    if let Ok((mappings, _)) = dbg.mappings(64) {
        let mut hdr = [0u8; 40];
        for m in mappings {
            if (m.permission & services::debug::PERM_EXECUTE) == 0 {
                continue;
            }
            let base = m.base_address;
            if dbg.read(base.wrapping_add(0x1490_a2a0), &mut hdr).is_ok() {
                let buf_ptr = u64::from_le_bytes([
                    hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
                ]);
                let buf_sz = u64::from_le_bytes([
                    hdr[16], hdr[17], hdr[18], hdr[19], hdr[20], hdr[21], hdr[22], hdr[23],
                ]);
                let data_sz = u64::from_le_bytes([
                    hdr[24], hdr[25], hdr[26], hdr[27], hdr[28], hdr[29], hdr[30], hdr[31],
                ]);
                if buf_sz == 0x1000
                    && data_sz == 0x800
                    && buf_ptr >= 0x10_0000
                    && (buf_ptr & 0xfff) == 0
                {
                    let mut w_bytes = [0u8; 8];
                    if dbg.read(base.wrapping_add(0x1490_8e68), &mut w_bytes).is_ok() {
                        let w = u64::from_le_bytes(w_bytes);
                        if w > 0 {
                            let ring_base = base.wrapping_add(0x1491_2360);
                            let n = resample_ring_32k_to_48k(&dbg, ring_base, w, dst);
                            if n > 0 {
                                return n;
                            }
                        }
                    }
                }
            }
        }
    }

    let Ok(tids) = dbg.threads(32) else {
        return 0;
    };
    let mut stack = [0u8; 256];
    for tid in tids {
        let Ok(regs) = dbg.registers(tid) else {
            continue;
        };
        for stack_base in [regs.sp, regs.fp] {
            if stack_base < 0x10_0000 || dbg.read(stack_base, &mut stack).is_err() {
                continue;
            }
            for slot in 0..32 {
                let off = slot * 8;
                let cand = u64::from_le_bytes([
                    stack[off],
                    stack[off + 1],
                    stack[off + 2],
                    stack[off + 3],
                    stack[off + 4],
                    stack[off + 5],
                    stack[off + 6],
                    stack[off + 7],
                ]);
                if cand < 0x10_0000 || cand >= 0x80_0000_0000 || (cand & 7) != 0 {
                    continue;
                }
                let mut hdr = [0u8; 40];
                // Generic libnx `AudioOutBuffer` struct on stack.
                if dbg.read(cand, &mut hdr).is_ok() {
                    let buf_ptr = u64::from_le_bytes([
                        hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
                    ]);
                    let buf_sz = u64::from_le_bytes([
                        hdr[16], hdr[17], hdr[18], hdr[19], hdr[20], hdr[21], hdr[22], hdr[23],
                    ]);
                    let data_sz = u64::from_le_bytes([
                        hdr[24], hdr[25], hdr[26], hdr[27], hdr[28], hdr[29], hdr[30], hdr[31],
                    ]);
                    if (0x800..=0x20000).contains(&buf_sz)
                        && (buf_sz & 0x7ff) == 0
                        && data_sz > 0
                        && data_sz <= buf_sz
                        && buf_ptr >= 0x10_0000
                        && (buf_ptr & 0xff) == 0
                    {
                        let n = read_linear_pcm(&dbg, buf_ptr, data_sz as usize, dst);
                        if n > 0 {
                            return n;
                        }
                    }
                }
            }
        }
    }
    0
}

/// Prepares a retroactive audio replay of the last `duration_ms` (up to 5,000 ms) from the ring buffer
/// and returns [`Response::Data`] with the first chunk and `file_size` set to the total PCM byte length.
pub fn replay_begin(duration_ms: u32) -> Response {
    if duration_ms == 0 || duration_ms > MAX_RING_MS {
        return Response::error(
            ErrorKind::BadRequest,
            alloc::format!("audio replay duration must be 1-{MAX_RING_MS} ms"),
        );
    }
    // Attempt an immediate sample grab from the running application if the ring buffer hasn't been fed recently.
    let now = uptime_ms();
    let needs_poll = {
        let ring = RING.lock();
        ring.filled == 0 || now.saturating_sub(ring.last_push_ms) > 1_000
    };
    if needs_poll {
        let mut temp = Vec::new();
        if temp.try_reserve_exact(SAMPLES_PER_SECOND).is_ok() {
            temp.resize(SAMPLES_PER_SECOND, 0i16);
            let n = capture_from_running_app(&mut temp);
            if n > 0 {
                let _ = RING.lock().push_samples(&temp[..n], uptime_ms());
            }
        }
    }

    let mut ring = RING.lock();
    let _total_bytes = ring.prepare_replay(duration_ms);
    let (data, file_size) = ring.read_replay_chunk(0, MAX_CHUNK as u32);
    Response::Data { data, file_size }
}

/// Reads the next chunk of the prepared retroactive audio replay at `offset` (up to `len` bytes, capped at `MAX_CHUNK`).
pub fn replay_read(offset: u64, len: u32) -> Response {
    let ring = RING.lock();
    if ring.replay_bytes == 0 {
        return Response::error(
            ErrorKind::BadRequest,
            "no audio replay prepared on this console; send AudioReplay first",
        );
    }
    let (data, file_size) = ring.read_replay_chunk(offset, len);
    Response::Data { data, file_size }
}

/// Captures a single live PCM chunk of `duration_ms` (clamped to `20..=MAX_STREAM_CHUNK_MS`, ~32 KiB default)
/// for proactive `record_audio` streaming, pushing the captured samples into the ring buffer as well.
///
/// Waits `duration_ms` so the running application generates a fresh window of samples, then reads them from the
/// running application (or returns zeroed silence frames if no application audio is active).
pub fn stream_chunk(duration_ms: u32) -> Response {
    let ms = if duration_ms == 0 {
        DEFAULT_STREAM_CHUNK_MS
    } else {
        duration_ms.clamp(20, MAX_STREAM_CHUNK_MS)
    };
    let frames = ((ms as usize) * (SAMPLE_RATE as usize)) / 1000;
    let sample_count = frames * (CHANNELS as usize);
    let byte_len = sample_count * BYTES_PER_SAMPLE;

    let mut pcm_samples = Vec::new();
    if pcm_samples.try_reserve_exact(sample_count).is_err() {
        return Response::error(
            ErrorKind::OutOfMemory,
            "no room for audio streaming chunk in sysmodule heap",
        );
    }
    pcm_samples.resize(sample_count, 0i16);

    let sleep_ns = (ms as i64) * 1_000_000;
    let _ = svc::sleep_thread(sleep_ns);
    let _captured = capture_from_running_app(&mut pcm_samples);

    let _ = RING.lock().push_samples(&pcm_samples, uptime_ms());

    let mut data = Vec::with_capacity(byte_len);
    for s in pcm_samples {
        data.extend_from_slice(&s.to_le_bytes());
    }
    Response::Data {
        data,
        file_size: BYTES_PER_SECOND as u64,
    }
}

/// Queries the console's current audio output status (`sample_rate`, `channels`, `target`, `volume`, `muted`).
pub fn status() -> AudioStatus {
    services::audio::status().unwrap_or_default()
}

/// Sets the system master volume (`0.0..=1.0`) and/or mute state, returning the updated [`AudioStatus`].
pub fn configure(volume: Option<f32>, muted: Option<bool>) -> Result<AudioStatus, nx::result::ResultCode> {
    if let Some(v) = volume {
        let _ = services::audio::set_volume(v)?;
    }
    if let Some(m) = muted {
        let _ = services::audio::set_mute(m)?;
    }
    services::audio::status()
}
