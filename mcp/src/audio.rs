// SPDX-License-Identifier: GPL-3.0-only
#![allow(dead_code)]
//! PC-side WAV encoding/decoding, DSP waveform & glitch analysis, reference SFX cross-correlation,
//! spectrogram rendering, and modality-aware MCP tool output formatting.

use std::fmt::Write as _;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ImageEncoder, Rgb, RgbImage};
use rmcp::model::ContentBlock;
use serde::{Deserialize, Serialize};

use crate::modalities::{AudioFormat, Modalities};

/// Standard Nintendo Switch audio output sample rate (48,000 Hz).
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// Standard stereo channel count.
pub const DEFAULT_CHANNELS: u16 = 2;
/// Standard 44-byte RIFF/WAVE header size for 16-bit PCM.
pub const WAV_HEADER_SIZE: usize = 44;
/// RMS level below which a capture is classified as silent (-60.0 dBFS).
pub const SILENCE_THRESHOLD_DB: f64 = -60.0;
/// Sample-to-sample jump threshold (as fraction of full scale) indicating a phase discontinuity / glitch.
pub const GLITCH_JUMP_THRESHOLD: f64 = 0.6;
/// Normalized cross-correlation threshold for asserting that a reference sound effect occurred.
pub const REFERENCE_MATCH_THRESHOLD: f64 = 0.65;

/// Decoded 16-bit PCM waveform data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavData {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved signed 16-bit PCM samples.
    pub samples: Vec<i16>,
}

impl WavData {
    /// Number of multi-channel sample frames.
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / (self.channels as usize)
        }
    }

    /// Duration of the waveform in seconds.
    pub fn duration_s(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames() as f64 / (self.sample_rate as f64)
        }
    }
}

/// Result of cross-correlating a reference `.wav` asset against a captured waveform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceMatch {
    /// Whether `confidence >= REFERENCE_MATCH_THRESHOLD`.
    pub detected: bool,
    /// Best-matching onset offset inside the captured recording, in milliseconds.
    pub timestamp_ms: f64,
    /// Peak normalized cross-correlation score in `[0.0, 1.0]`.
    pub confidence: f64,
}

/// Comprehensive DSP analysis of a captured audio buffer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioAnalysis {
    pub duration_s: f64,
    pub sample_rate: u32,
    pub channels: u16,
    /// Root-mean-square level in dBFS (`[-120.0, 0.0]`).
    pub rms_db: f64,
    /// Peak sample magnitude in dBFS (`[-120.0, 0.0]`).
    pub peak_db: f64,
    /// `true` when `rms_db < SILENCE_THRESHOLD_DB` (-60.0 dBFS).
    pub silent: bool,
    /// Number of samples hitting full-scale clipping (`i16::MAX` or `i16::MIN`).
    pub clipping_samples: usize,
    /// Count of sharp phase discontinuities (`> 0.6` full-scale single-sample jump) and 20ms starvation loops.
    pub underrun_glitches: usize,
    /// Estimated dominant frequency in Hz (`0.0` if silent).
    pub dominant_freq_hz: f64,
    /// Optional reference `.wav` cross-correlation result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_match: Option<ReferenceMatch>,
}

impl AudioAnalysis {
    /// Formats a human- and AI-readable single-block diagnostic summary.
    pub fn summary_text(&self, saved_wav: Option<&Path>) -> String {
        let mut out = String::new();
        if let Some(path) = saved_wav {
            let _ = writeln!(out, "saved audio: {}", path.display());
        }
        let state = if self.silent { "SILENT" } else { "ACTIVE" };
        let _ = writeln!(
            out,
            "audio: {state} ({:.2}s, {} Hz, {} ch) | RMS {:.1} dBFS, peak {:.1} dBFS | dominant {:.1} Hz | clipping: {} samples, underrun glitches: {}",
            self.duration_s,
            self.sample_rate,
            self.channels,
            self.rms_db,
            self.peak_db,
            self.dominant_freq_hz,
            self.clipping_samples,
            self.underrun_glitches,
        );
        if let Some(ref_match) = &self.reference_match {
            let verdict = if ref_match.detected { "DETECTED" } else { "NOT DETECTED" };
            let _ = writeln!(
                out,
                "reference match: {verdict} (confidence {:.3}, timestamp {:.1} ms)",
                ref_match.confidence, ref_match.timestamp_ms
            );
        }
        out.trim_end().to_string()
    }
}

/// Live audio recorded by [`crate::client::Client::record_audio`].
#[derive(Debug)]
pub struct Recording {
    pub samples: Vec<i16>,
    /// Silence filled in where the stream had gaps (the console's ring overflowed, or the sound paused).
    pub gaps_ms: u64,
    /// Why it ended before the length asked for, if it did.
    pub stopped: Option<String>,
}

/// What to do so a homebrew program can be heard: the one call that opts in to gameplay recording.
pub const OPT_IN: &str = "call appletInitializeGamePlayRecording() once at startup (libnx; it hands 96 MiB of the program's memory to the recorder, so it needs the full memory of title takeover)";

/// Why the console isn't capturing audio, and what to do about it; `None` while it is.
pub fn why_silent(c: &proto::AudioCapture) -> Option<String> {
    use proto::CaptureState as S;
    let title = c.program_id.map_or_else(|| "the running title".to_string(), |id| format!("title {id:016x}"));
    let homebrew = c.hbl == Some(true);
    Some(match &c.state {
        S::Capturing => return None,
        S::Unsupported => "the Kickstand NRO is serving instead of the background sysmodule, and only the sysmodule captures audio: exit the NRO (+) and try again".into(),
        S::NoApplication => "no game or homebrew is running (the home menu's own sounds aren't recordable): start it first, then capture".into(),
        S::Busy { holder } => format!(
            "{holder} is running on the console and reads the same gameplay recorder, so Kickstand stays off it (two readers would each get half the sound): turn {holder} off, or remove its boot2 flag and reboot"
        ),
        S::Failed { result, during } => format!("the console's gameplay recorder (grc:d) failed while {during}: result {result:#x}"),
        S::Held => "gameplay capture is paused while Kickstand restarts its background service; try again in a moment".into(),
        S::NeedsReboot { ms } => format!(
            "{title} is running, but the gameplay recorder has sent nothing for {:.1} s, and Kickstand's background service was restarted since it last read the recorder: the console's recorder (grc) sends nothing to a restarted reader until the console reboots. If the game is in front (not the home menu or an applet such as Controllers), reboot the console to capture again",
            f64::from(*ms) / 1000.0
        ),
        S::Waiting { ms } => {
            let quiet = if *ms >= 1_000 { format!(" for {:.1} s", f64::from(*ms) / 1000.0) } else { String::new() };
            let base = if homebrew {
                format!("homebrew is running in place of {title}, but the gameplay recorder has sent no sound{quiet}")
            } else {
                format!("{title} is running, but the gameplay recorder has sent no sound{quiet}")
            };
            let reason = match c.video_capture {
                Some(0) => format!(
                    "{title} turns video capture off in its control data, so nothing it plays{} can be recorded. {}",
                    if homebrew { " (or homebrew in its place)" } else { "" },
                    if homebrew { "Run the homebrew in place of a game that allows video capture (in that game, holding Capture saves a clip)" } else { "Kickstand can't hear this game" }
                ),
                Some(1) if homebrew => format!(
                    "{title} sets video capture to Manual, so the program in it must opt in: in the homebrew, {OPT_IN}. Or run it in place of a game whose recording is automatic"
                ),
                Some(1) => format!("{title} sets video capture to Manual and hasn't started recording here (some games only record in parts)"),
                Some(2) => "recording is automatic for this title, so it may be suspended behind the home menu, still starting, or in a part that doesn't record; try again in a moment".to_string(),
                _ if homebrew => format!(
                    "homebrew is only heard when it records gameplay: run it in place of a game that allows video capture, or, if that game's recording is Manual, {OPT_IN}"
                ),
                _ => "the game may not allow video capture (holding Capture would save a clip if it did), or it's suspended behind the home menu".to_string(),
            };
            format!("{base}: {reason}")
        }
    })
}

/// One line on the capture, for replies: how much is buffered and how fresh it is.
pub fn capture_line(c: &proto::AudioCapture) -> String {
    let age = c.newest_age_ms.map_or_else(String::new, |ms| {
        if ms < 500 { ", live".to_string() } else { format!(", newest from {:.1} s ago", f64::from(ms) / 1000.0) }
    });
    format!("console audio capture: {:.1} s buffered{age}", f64::from(c.buffered_ms) / 1000.0)
}

/// Encodes interleaved 16-bit signed PCM samples into a standard 44-byte RIFF/WAVE byte buffer.
pub fn encode_wav(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_bytes = samples.len() * 2;
    let riff_size = (36usize.saturating_add(data_bytes)).min(u32::MAX as usize) as u32;
    let block_align = channels.saturating_mul(2);
    let byte_rate = sample_rate.saturating_mul(u32::from(block_align));

    let mut out = Vec::with_capacity(WAV_HEADER_SIZE + data_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // AudioFormat = 1 (PCM)
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // BitsPerSample = 16
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes.min(u32::MAX as usize) as u32).to_le_bytes());
    for &sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Encodes raw little-endian 16-bit PCM bytes into a standard 44-byte RIFF/WAVE byte buffer.
pub fn encode_wav_bytes(pcm_le: &[u8], sample_rate: u32, channels: u16) -> Vec<u8> {
    let aligned_len = pcm_le.len() & !1;
    let data_bytes = aligned_len;
    let riff_size = (36usize.saturating_add(data_bytes)).min(u32::MAX as usize) as u32;
    let block_align = channels.saturating_mul(2);
    let byte_rate = sample_rate.saturating_mul(u32::from(block_align));

    let mut out = Vec::with_capacity(WAV_HEADER_SIZE + data_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes.min(u32::MAX as usize) as u32).to_le_bytes());
    out.extend_from_slice(&pcm_le[..aligned_len]);
    out
}

/// Decodes a RIFF/WAVE byte slice into [`WavData`].
/// Supports standard 16-bit PCM as well as 8-bit, 24-bit, 32-bit PCM and 32-bit IEEE float WAV files.
pub fn decode_wav(bytes: &[u8]) -> Result<WavData> {
    if bytes.len() < WAV_HEADER_SIZE {
        bail!("WAV data too short ({} bytes, need at least 44)", bytes.len());
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("invalid WAV header: missing RIFF/WAVE magic");
    }

    let mut pos = 12usize;
    let mut audio_format: Option<u16> = None;
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut bits_per_sample: u16 = 0;
    let mut data_slice: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = body_start.saturating_add(chunk_size).min(bytes.len());

        if chunk_id == b"fmt " {
            if body_end - body_start < 16 {
                bail!("WAV fmt chunk too short");
            }
            let fmt = &bytes[body_start..body_end];
            audio_format = Some(u16::from_le_bytes([fmt[0], fmt[1]]));
            channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            bits_per_sample = u16::from_le_bytes([fmt[14], fmt[15]]);
        } else if chunk_id == b"data" {
            data_slice = Some(&bytes[body_start..body_end]);
            break;
        }

        // RIFF chunks are word-aligned (padded to even byte boundary).
        let advance = chunk_size + (chunk_size & 1);
        pos = body_start.saturating_add(advance);
    }

    let format = audio_format.context("WAV file missing fmt chunk")?;
    let raw_data = data_slice.context("WAV file missing data chunk")?;
    if channels == 0 || sample_rate == 0 {
        bail!("WAV header has invalid channels ({channels}) or sample_rate ({sample_rate})");
    }

    let samples: Vec<i16> = match (format, bits_per_sample) {
        (1, 16) => raw_data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect(),
        (1, 8) => raw_data
            .iter()
            .map(|&b| (i16::from(b) - 128) * 256)
            .collect(),
        (1, 24) => raw_data
            .chunks_exact(3)
            .map(|c| i16::from_le_bytes([c[1], c[2]]))
            .collect(),
        (1, 32) => raw_data
            .chunks_exact(4)
            .map(|c| i16::from_le_bytes([c[2], c[3]]))
            .collect(),
        (3, 32) => raw_data
            .chunks_exact(4)
            .map(|c| {
                let f = f32::from_le_bytes([c[0], c[1], c[2], c[3]]).clamp(-1.0, 1.0);
                (f * 32767.0) as i16
            })
            .collect(),
        _ => bail!("unsupported WAV format (format={format}, bits_per_sample={bits_per_sample})"),
    };

    Ok(WavData {
        sample_rate,
        channels,
        samples,
    })
}

/// Writes interleaved `i16` PCM samples to a `.wav` file on disk.
pub fn write_wav_file(path: &Path, samples: &[i16], sample_rate: u32, channels: u16) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let wav = encode_wav(samples, sample_rate, channels);
    std::fs::write(path, wav).with_context(|| format!("writing WAV file {}", path.display()))
}

/// Reads and decodes a `.wav` file from disk.
pub fn read_wav_file(path: &Path) -> Result<WavData> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading WAV file {}", path.display()))?;
    decode_wav(&bytes)
}

/// Converts little-endian `i16` PCM byte slice into a `Vec<i16>`.
pub fn pcm_bytes_to_i16(pcm_le: &[u8]) -> Vec<i16> {
    pcm_le
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Downmixes interleaved multi-channel `i16` samples into normalized mono `f64` samples in `[-1.0, 1.0]`.
pub fn downmix_mono_norm(samples: &[i16], channels: u16) -> Vec<f64> {
    let ch = (channels as usize).max(1);
    samples
        .chunks_exact(ch)
        .map(|frame| {
            let sum: f64 = frame.iter().map(|&s| f64::from(s) / 32768.0).sum();
            sum / (ch as f64)
        })
        .collect()
}

/// Estimates the dominant frequency in Hz of a mono normalized signal using a Hann-windowed DFT
/// with parabolic peak interpolation over the highest-energy window.
pub fn estimate_dominant_freq_hz(mono: &[f64], sample_rate: u32) -> f64 {
    if mono.len() < 64 || sample_rate == 0 {
        return 0.0;
    }

    // Select a window of up to 4096 frames around the highest-energy region.
    let win_len = mono.len().min(4096);
    let step = (win_len / 2).max(1);
    let mut best_start = 0usize;
    let mut best_energy = -1.0f64;
    let mut start = 0usize;
    while start + win_len <= mono.len() {
        let energy: f64 = mono[start..start + win_len].iter().map(|&x| x * x).sum();
        if energy > best_energy {
            best_energy = energy;
            best_start = start;
        }
        if start + win_len == mono.len() {
            break;
        }
        start = (start + step).min(mono.len() - win_len);
    }

    let segment = &mono[best_start..best_start + win_len];
    let mean: f64 = segment.iter().copied().sum::<f64>() / (win_len as f64);

    // Apply Hann window and remove DC offset.
    let n_f64 = win_len as f64;
    let windowed: Vec<f64> = segment
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * (i as f64) / (n_f64 - 1.0)).cos());
            (x - mean) * w
        })
        .collect();

    let min_bin = ((20.0 * n_f64 / f64::from(sample_rate)).ceil() as usize).max(1);
    let max_bin = ((12_000.0 * n_f64 / f64::from(sample_rate)).floor() as usize)
        .min(win_len / 2 - 1)
        .max(min_bin);

    let mut mags = vec![0.0f64; max_bin + 2];
    let mut peak_bin = min_bin;
    let mut peak_mag = -1.0f64;

    for (k, mag_slot) in mags.iter_mut().enumerate().take(max_bin + 1).skip(min_bin) {
        let omega = 2.0 * std::f64::consts::PI * (k as f64) / n_f64;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &x in &windowed {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        let mag = power.max(0.0).sqrt();
        *mag_slot = mag;
        if mag > peak_mag {
            peak_mag = mag;
            peak_bin = k;
        }
    }

    if peak_mag <= 0.0 {
        return 0.0;
    }

    // Compute one extra bin at `max_bin + 1` if needed for parabolic interpolation.
    if peak_bin == max_bin && max_bin + 1 < win_len / 2 {
        let k = max_bin + 1;
        let omega = 2.0 * std::f64::consts::PI * (k as f64) / n_f64;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &x in &windowed {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        mags[k] = power.max(0.0).sqrt();
    }

    // Parabolic interpolation on log/linear magnitudes around `peak_bin`.
    let delta = if peak_bin > min_bin && peak_bin <= max_bin {
        let alpha = mags[peak_bin - 1];
        let beta = mags[peak_bin];
        let gamma = mags[peak_bin + 1];
        let denom = alpha - 2.0 * beta + gamma;
        if denom.abs() > 1e-12 {
            0.5 * (alpha - gamma) / denom
        } else {
            0.0
        }
    } else {
        0.0
    };

    let refined_bin = (peak_bin as f64) + delta.clamp(-0.5, 0.5);
    refined_bin * f64::from(sample_rate) / n_f64
}

/// Detects sharp sample-to-sample phase discontinuities (`> 0.6` full scale) and 20ms buffer starvation
/// buzz loops or zero-dropouts inside active audio.
pub fn count_underrun_glitches(samples: &[i16], sample_rate: u32, channels: u16) -> usize {
    let ch = (channels as usize).max(1);
    if samples.len() < ch * 2 {
        return 0;
    }

    let mut glitches = 0usize;
    let jump_limit = (GLITCH_JUMP_THRESHOLD * 32768.0) as i32;

    // 1. Sample-to-sample discontinuity check per channel (with 2 ms refractory window so one click counts once).
    let refractory_frames = ((sample_rate as usize) / 500).max(1);
    let mut cooldown = 0usize;
    for frame_idx in 1..(samples.len() / ch) {
        if cooldown > 0 {
            cooldown -= 1;
            continue;
        }
        let prev_base = (frame_idx - 1) * ch;
        let curr_base = frame_idx * ch;
        let mut jumped = false;
        for c in 0..ch {
            let diff = (i32::from(samples[curr_base + c]) - i32::from(samples[prev_base + c])).abs();
            if diff >= jump_limit {
                jumped = true;
                break;
            }
        }
        if jumped {
            glitches += 1;
            cooldown = refractory_frames;
        }
    }

    // 2. Sudden zero-dropout gaps (~5ms..40ms of exact zeros flanked by loud signal) from DMA buffer underruns.
    let mono = downmix_mono_norm(samples, channels);
    let block_frames = ((sample_rate as usize) / 200).max(16); // 5 ms blocks
    if mono.len() >= block_frames * 4 {
        let block_rms: Vec<f64> = mono
            .chunks_exact(block_frames)
            .map(|b| (b.iter().map(|&x| x * x).sum::<f64>() / (b.len() as f64)).sqrt())
            .collect();
        let mut i = 1usize;
        while i + 1 < block_rms.len() {
            if block_rms[i - 1] > 0.05 && block_rms[i] < 1e-5 {
                let mut zero_run = 1usize;
                while i + zero_run < block_rms.len() && block_rms[i + zero_run] < 1e-5 {
                    zero_run += 1;
                }
                if zero_run <= 8 && i + zero_run < block_rms.len() && block_rms[i + zero_run] > 0.05 {
                    glitches += 1;
                }
                i += zero_run;
            } else {
                i += 1;
            }
        }
    }

    glitches
}

/// Analyzes interleaved 16-bit PCM samples and computes [`AudioAnalysis`].
pub fn analyze_pcm(
    samples: &[i16],
    sample_rate: u32,
    channels: u16,
    reference: Option<&WavData>,
) -> AudioAnalysis {
    let ch = (channels as usize).max(1);
    let frames = samples.len() / ch;
    let duration_s = if sample_rate > 0 {
        (frames as f64) / f64::from(sample_rate)
    } else {
        0.0
    };

    if samples.is_empty() {
        return AudioAnalysis {
            duration_s: 0.0,
            sample_rate,
            channels,
            rms_db: -120.0,
            peak_db: -120.0,
            silent: true,
            clipping_samples: 0,
            underrun_glitches: 0,
            dominant_freq_hz: 0.0,
            reference_match: reference.map(|_| ReferenceMatch {
                detected: false,
                timestamp_ms: 0.0,
                confidence: 0.0,
            }),
        };
    }

    let mut sum_sq = 0.0f64;
    let mut peak_abs = 0.0f64;
    let mut clipping_samples = 0usize;

    for &s in samples {
        if s == i16::MAX || s == i16::MIN {
            clipping_samples += 1;
        }
        let norm = f64::from(s) / 32768.0;
        let abs = (f64::from(s).abs()) / 32767.0;
        sum_sq += norm * norm;
        if abs > peak_abs {
            peak_abs = abs;
        }
    }

    let rms = (sum_sq / (samples.len() as f64)).sqrt();
    let rms_db = if rms <= 1e-6 {
        -120.0
    } else {
        (20.0 * rms.log10()).clamp(-120.0, 0.0)
    };
    let peak_db = if peak_abs <= 1e-6 {
        -120.0
    } else {
        (20.0 * peak_abs.log10()).clamp(-120.0, 0.0)
    };
    let silent = rms_db < SILENCE_THRESHOLD_DB;

    let mono = downmix_mono_norm(samples, channels);
    let dominant_freq_hz = if silent {
        0.0
    } else {
        estimate_dominant_freq_hz(&mono, sample_rate)
    };
    let underrun_glitches = if silent {
        0
    } else {
        count_underrun_glitches(samples, sample_rate, channels)
    };

    let reference_match = reference.map(|ref_wav| {
        let captured = WavData {
            sample_rate,
            channels,
            samples: samples.to_vec(),
        };
        match_reference(&captured, ref_wav)
    });

    AudioAnalysis {
        duration_s,
        sample_rate,
        channels,
        rms_db,
        peak_db,
        silent,
        clipping_samples,
        underrun_glitches,
        dominant_freq_hz,
        reference_match,
    }
}

/// Linearly resamples a mono signal from `src_rate` to `dst_rate`.
fn resample_mono(mono: &[f64], src_rate: u32, dst_rate: u32) -> Vec<f64> {
    if src_rate == dst_rate || mono.is_empty() || src_rate == 0 || dst_rate == 0 {
        return mono.to_vec();
    }
    let ratio = f64::from(src_rate) / f64::from(dst_rate);
    let out_len = ((mono.len() as f64) / ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = (i as f64) * ratio;
        let idx = src_pos.floor() as usize;
        let frac = src_pos - (idx as f64);
        let a = mono[idx.min(mono.len() - 1)];
        let b = mono[(idx + 1).min(mono.len() - 1)];
        out.push(a + frac * (b - a));
    }
    out
}

/// In-place iterative radix-2 FFT (`re.len()` a power of two); `inverse` scales by `1/n`.
fn fft(re: &mut [f64], im: &mut [f64], inverse: bool) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut len = 2;
    while len <= n {
        let angle = sign * 2.0 * std::f64::consts::PI / len as f64;
        let (w_re, w_im) = (angle.cos(), angle.sin());
        for start in (0..n).step_by(len) {
            let (mut cur_re, mut cur_im) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let t_re = re[b] * cur_re - im[b] * cur_im;
                let t_im = re[b] * cur_im + im[b] * cur_re;
                re[b] = re[a] - t_re;
                im[b] = im[a] - t_im;
                re[a] += t_re;
                im[a] += t_im;
                let next = cur_re * w_re - cur_im * w_im;
                cur_im = cur_re * w_im + cur_im * w_re;
                cur_re = next;
            }
        }
        len <<= 1;
    }
    if inverse {
        let scale = 1.0 / n as f64;
        re.iter_mut().for_each(|x| *x *= scale);
        im.iter_mut().for_each(|x| *x *= scale);
    }
}

/// `sum_i x[lag + i] * y[i]` for every lag in `0..=x.len() - y.len()`, through the FFT.
fn cross_correlate(x: &[f64], y: &[f64]) -> Vec<f64> {
    let n = (x.len() + y.len()).next_power_of_two();
    let (mut xr, mut xi) = (vec![0.0; n], vec![0.0; n]);
    let (mut yr, mut yi) = (vec![0.0; n], vec![0.0; n]);
    xr[..x.len()].copy_from_slice(x);
    yr[..y.len()].copy_from_slice(y);
    fft(&mut xr, &mut xi, false);
    fft(&mut yr, &mut yi, false);
    // X * conj(Y)
    for k in 0..n {
        let (a, b, c, d) = (xr[k], xi[k], yr[k], -yi[k]);
        xr[k] = a * c - b * d;
        xi[k] = a * d + b * c;
    }
    fft(&mut xr, &mut xi, true);
    xr.truncate(x.len() - y.len() + 1);
    xr
}

/// A window this much quieter than the reference (power ratio, -30 dB) can't hold that sound, however well
/// its shape happens to correlate (correlation ignores loudness, so near-silence would otherwise match).
const MIN_WINDOW_POWER_RATIO: f64 = 1e-3;

/// Finds `reference` (e.g. an SFX asset) in `captured` by normalized cross-correlation over every lag
/// (computed exactly through the FFT): where it matches best, and how well (`confidence`, the Pearson
/// correlation of that window with the reference, `0..=1`). Both are mixed down to mono, and the reference
/// is resampled to the capture's rate. A window much quieter than the reference never matches.
pub fn match_reference(captured: &WavData, reference: &WavData) -> ReferenceMatch {
    let none = ReferenceMatch { detected: false, timestamp_ms: 0.0, confidence: 0.0 };
    let cap = downmix_mono_norm(&captured.samples, captured.channels);
    let raw = downmix_mono_norm(&reference.samples, reference.channels);
    let refm = resample_mono(&raw, reference.sample_rate, captured.sample_rate);
    let (n, m) = (cap.len(), refm.len());
    if m < 16 || n < m || captured.sample_rate == 0 {
        return none;
    }
    let mean = refm.iter().sum::<f64>() / m as f64;
    let ref_zero: Vec<f64> = refm.iter().map(|x| x - mean).collect();
    let ref_energy: f64 = ref_zero.iter().map(|x| x * x).sum();
    if ref_energy <= 1e-12 {
        return none;
    }
    // Window sums for each lag's mean and variance.
    let mut sum = vec![0.0f64; n + 1];
    let mut sq = vec![0.0f64; n + 1];
    for i in 0..n {
        sum[i + 1] = sum[i] + cap[i];
        sq[i + 1] = sq[i] + cap[i] * cap[i];
    }
    // sum(x * (r - mean_r)) == sum((x - mean_x) * (r - mean_r)), since sum(r - mean_r) == 0.
    let dots = cross_correlate(&cap, &ref_zero);
    let mut best = (0usize, 0.0f64);
    for (lag, dot) in dots.iter().enumerate() {
        let s = sum[lag + m] - sum[lag];
        let var = (sq[lag + m] - sq[lag] - s * s / m as f64).max(0.0);
        if var < ref_energy * MIN_WINDOW_POWER_RATIO {
            continue;
        }
        let score = (dot / (var * ref_energy).sqrt()).abs().min(1.0);
        if score > best.1 {
            best = (lag, score);
        }
    }
    ReferenceMatch {
        detected: best.1 >= REFERENCE_MATCH_THRESHOLD,
        timestamp_ms: best.0 as f64 * 1000.0 / f64::from(captured.sample_rate),
        confidence: best.1,
    }
}
/// Loads a reference `.wav` file from `reference_path` and cross-correlates it against `captured`.
pub fn match_reference_file(captured: &WavData, reference_path: &Path) -> Result<ReferenceMatch> {
    let ref_wav = read_wav_file(reference_path)?;
    Ok(match_reference(captured, &ref_wav))
}

/// Maps a normalized `[0.0, 1.0]` spectrogram intensity to an Inferno-inspired perceptual RGB color.
fn spectrogram_colormap(t: f64) -> Rgb<u8> {
    let x = t.clamp(0.0, 1.0);
    let (r, g, b) = if x < 0.25 {
        let u = x / 0.25;
        (10.0 + 60.0 * u, 12.0 + 18.0 * u, 28.0 + 95.0 * u)
    } else if x < 0.55 {
        let u = (x - 0.25) / 0.30;
        (70.0 + 125.0 * u, 30.0 + 35.0 * u, 123.0 - 25.0 * u)
    } else if x < 0.82 {
        let u = (x - 0.55) / 0.27;
        (195.0 + 55.0 * u, 65.0 + 125.0 * u, 98.0 - 70.0 * u)
    } else {
        let u = (x - 0.82) / 0.18;
        (250.0 + 5.0 * u, 190.0 + 62.0 * u, 28.0 + 190.0 * u)
    };
    Rgb([r as u8, g as u8, b as u8])
}

/// Renders a combined waveform envelope + STFT frequency spectrogram [`RgbImage`] (`800x400`).
pub fn render_spectrogram_image(samples: &[i16], sample_rate: u32, channels: u16) -> RgbImage {
    const W: u32 = 800;
    const H: u32 = 400;
    const WAVE_TOP: u32 = 12;
    const WAVE_BOTTOM: u32 = 132;
    const SPEC_TOP: u32 = 148;
    const SPEC_BOTTOM: u32 = H - 12;

    let mut img = RgbImage::from_pixel(W, H, Rgb([16, 18, 24]));
    let mono = downmix_mono_norm(samples, channels);

    // Draw panel borders and center zero line for waveform envelope.
    let wave_mid = (WAVE_TOP + WAVE_BOTTOM) / 2;
    for x in 0..W {
        img.put_pixel(x, WAVE_TOP, Rgb([45, 50, 64]));
        img.put_pixel(x, wave_mid, Rgb([38, 44, 58]));
        img.put_pixel(x, WAVE_BOTTOM, Rgb([45, 50, 64]));
        img.put_pixel(x, SPEC_TOP, Rgb([45, 50, 64]));
        img.put_pixel(x, SPEC_BOTTOM, Rgb([45, 50, 64]));
    }

    if mono.is_empty() {
        return img;
    }

    // 1. Render top waveform min/max peak + RMS envelope.
    let wave_h = (WAVE_BOTTOM - WAVE_TOP) as f64;
    let half_h = wave_h * 0.48;
    for px in 0..W {
        let start = ((px as usize) * mono.len()) / (W as usize);
        let end = (((px as usize + 1) * mono.len()) / (W as usize))
            .max(start + 1)
            .min(mono.len());
        let slice = &mono[start..end];
        let mut min_v = 1.0f64;
        let mut max_v = -1.0f64;
        let mut sum_sq = 0.0f64;
        for &v in slice {
            if v < min_v {
                min_v = v;
            }
            if v > max_v {
                max_v = v;
            }
            sum_sq += v * v;
        }
        let rms_v = (sum_sq / (slice.len() as f64)).sqrt();
        let y_top = ((wave_mid as f64) - max_v.clamp(-1.0, 1.0) * half_h).round() as u32;
        let y_bot = ((wave_mid as f64) - min_v.clamp(-1.0, 1.0) * half_h).round() as u32;
        let y_rms_top = ((wave_mid as f64) - rms_v.clamp(0.0, 1.0) * half_h).round() as u32;
        let y_rms_bot = ((wave_mid as f64) + rms_v.clamp(0.0, 1.0) * half_h).round() as u32;

        let clipped = max_v >= 0.999 || min_v <= -0.999;
        let peak_color = if clipped {
            Rgb([245, 75, 75])
        } else {
            Rgb([56, 178, 196])
        };
        let rms_color = if clipped {
            Rgb([255, 120, 100])
        } else {
            Rgb([98, 230, 172])
        };

        for y in y_top.clamp(WAVE_TOP + 1, WAVE_BOTTOM - 1)
            ..=y_bot.clamp(WAVE_TOP + 1, WAVE_BOTTOM - 1)
        {
            img.put_pixel(px, y, peak_color);
        }
        for y in y_rms_top.clamp(WAVE_TOP + 1, WAVE_BOTTOM - 1)
            ..=y_rms_bot.clamp(WAVE_TOP + 1, WAVE_BOTTOM - 1)
        {
            img.put_pixel(px, y, rms_color);
        }
    }

    // 2. Render bottom STFT frequency spectrogram (0 Hz .. 12 kHz).
    let spec_h = SPEC_BOTTOM - SPEC_TOP - 1;
    let fft_size = 256usize;
    let max_freq_hz = 12_000.0f64.min(f64::from(sample_rate) * 0.5);
    let max_bin = (((max_freq_hz * (fft_size as f64)) / f64::from(sample_rate.max(1))).round()
        as usize)
        .clamp(8, fft_size / 2);

    // Precompute Hann window and cosine/sine tables for `1..=max_bin`.
    let hann: Vec<f64> = (0..fft_size)
        .map(|n| {
            0.5 * (1.0 - (2.0 * std::f64::consts::PI * (n as f64) / ((fft_size - 1) as f64)).cos())
        })
        .collect();
    let coeffs: Vec<f64> = (0..=max_bin)
        .map(|k| 2.0 * (2.0 * std::f64::consts::PI * (k as f64) / (fft_size as f64)).cos())
        .collect();

    let mut windowed = vec![0.0f64; fft_size];
    let mut bin_db = vec![-90.0f64; max_bin + 1];

    for px in 0..W {
        let center = ((px as usize) * mono.len()) / (W as usize);
        let start = center.saturating_sub(fft_size / 2);
        for i in 0..fft_size {
            let sample = mono.get(start + i).copied().unwrap_or(0.0);
            windowed[i] = sample * hann[i];
        }

        for k in 1..=max_bin {
            let coeff = coeffs[k];
            let (mut s1, mut s2) = (0.0f64, 0.0f64);
            for &x in &windowed {
                let s0 = x + coeff * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(1e-12);
            let mag = power.sqrt() / (fft_size as f64 * 0.5);
            bin_db[k] = (20.0 * mag.log10()).clamp(-80.0, 0.0);
        }
        bin_db[0] = bin_db[1];

        for row in 0..spec_h {
            // Map row 0 (top) -> max_bin (high freq), row spec_h - 1 (bottom) -> bin 1 (low freq).
            let norm_freq = 1.0 - (row as f64) / ((spec_h.max(2) - 1) as f64);
            let bin_pos = 1.0 + norm_freq * ((max_bin - 1) as f64);
            let b0 = (bin_pos.floor() as usize).clamp(1, max_bin);
            let b1 = (b0 + 1).min(max_bin);
            let frac = bin_pos - (b0 as f64);
            let db = bin_db[b0] + frac * (bin_db[b1] - bin_db[b0]);
            let intensity = ((db + 75.0) / 75.0).clamp(0.0, 1.0);
            img.put_pixel(px, SPEC_TOP + 1 + row, spectrogram_colormap(intensity));
        }
    }

    img
}

/// Renders a waveform + frequency spectrogram as PNG bytes.
pub fn render_spectrogram_png(samples: &[i16], sample_rate: u32, channels: u16) -> Result<Vec<u8>> {
    let img = render_spectrogram_image(samples, sample_rate, channels);
    let mut out = Vec::new();
    PngEncoder::new(&mut out).write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(out)
}

/// Renders a waveform + frequency spectrogram as JPEG bytes.
pub fn render_spectrogram_jpeg(
    samples: &[i16],
    sample_rate: u32,
    channels: u16,
) -> Result<Vec<u8>> {
    let img = render_spectrogram_image(samples, sample_rate, channels);
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img)?;
    Ok(out.into_inner())
}

/// Builds modality-aware MCP tool output blocks (`AudioFormat::Wav`, `AudioFormat::Spectrogram`, or
/// `AudioFormat::MetricsText`) from raw PCM samples.
///
/// Always writes the `.wav` file to `wav_path` on disk and computes [`AudioAnalysis`] (including optional
/// `reference_wav` cross-correlation).
pub fn format_audio_output(
    samples: &[i16],
    sample_rate: u32,
    channels: u16,
    wav_path: &Path,
    reference_wav: Option<&Path>,
    modalities: Modalities,
) -> Result<(AudioAnalysis, PathBuf, Vec<ContentBlock>)> {
    let wav_bytes = encode_wav(samples, sample_rate, channels);
    if let Some(parent) = wav_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    std::fs::write(wav_path, &wav_bytes)
        .with_context(|| format!("writing {}", wav_path.display()))?;

    let ref_data = match reference_wav {
        Some(path) => Some(read_wav_file(path)?),
        None => None,
    };
    let analysis = analyze_pcm(samples, sample_rate, channels, ref_data.as_ref());
    let summary = analysis.summary_text(Some(wav_path));

    let blocks = match modalities.preferred_audio() {
        AudioFormat::Wav => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&wav_bytes);
            vec![
                ContentBlock::text(summary),
                ContentBlock::audio(encoded, "audio/wav"),
            ]
        }
        AudioFormat::Spectrogram => {
            let spec_png = render_spectrogram_png(samples, sample_rate, channels)?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&spec_png);
            vec![
                ContentBlock::text(format!(
                    "{summary}\n[spectrogram + waveform envelope rendered for image modality]"
                )),
                ContentBlock::image(encoded, "image/png"),
            ]
        }
        AudioFormat::MetricsText => vec![ContentBlock::text(summary)],
    };

    Ok((analysis, wav_path.to_path_buf(), blocks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_440hz_wav_roundtrip_and_dsp_analysis() {
        let sample_rate = 48_000u32;
        let channels = 2u16;
        let frames = 24_000usize; // 0.5 seconds
        let amplitude = 16_000.0f64; // ~ -6.2 dBFS, no clipping
        let mut samples = Vec::with_capacity(frames * (channels as usize));

        for n in 0..frames {
            let t = (n as f64) / f64::from(sample_rate);
            let val = (amplitude * (2.0 * std::f64::consts::PI * 440.0 * t).sin()).round() as i16;
            samples.push(val);
            samples.push(val);
        }

        let wav_bytes = encode_wav(&samples, sample_rate, channels);
        assert_eq!(&wav_bytes[0..4], b"RIFF");
        assert_eq!(&wav_bytes[8..12], b"WAVE");
        assert_eq!(wav_bytes.len(), WAV_HEADER_SIZE + samples.len() * 2);

        let decoded = decode_wav(&wav_bytes).expect("valid WAV");
        assert_eq!(decoded.sample_rate, sample_rate);
        assert_eq!(decoded.channels, channels);
        assert_eq!(decoded.samples, samples);

        let analysis = analyze_pcm(&decoded.samples, decoded.sample_rate, decoded.channels, None);
        assert!(!analysis.silent, "440 Hz sine should not be silent");
        assert_eq!(analysis.clipping_samples, 0);
        assert_eq!(analysis.underrun_glitches, 0);
        assert!(
            (analysis.dominant_freq_hz - 440.0).abs() < 3.0,
            "expected ~440 Hz dominant frequency, got {:.2} Hz",
            analysis.dominant_freq_hz
        );
        assert!(
            analysis.rms_db > -12.0 && analysis.rms_db < -6.0,
            "unexpected rms_db: {}",
            analysis.rms_db
        );
    }

    #[test]
    fn silence_detection_on_zeroed_samples() {
        let samples = vec![0i16; 48_000 * 2];
        let analysis = analyze_pcm(&samples, 48_000, 2, None);
        assert!(analysis.silent);
        assert!(analysis.rms_db <= -100.0);
        assert_eq!(analysis.dominant_freq_hz, 0.0);
        assert_eq!(analysis.clipping_samples, 0);
        assert_eq!(analysis.underrun_glitches, 0);
    }

    #[test]
    fn glitch_and_clipping_detection_on_artificial_phase_jumps() {
        let sample_rate = 48_000u32;
        let channels = 2u16;
        let frames = 12_000usize; // 0.25s
        let mut samples = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let t = (n as f64) / f64::from(sample_rate);
            let val = (8_000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16;
            samples.push(val);
            samples.push(val);
        }

        // Inject artificial hard phase jumps (> 0.6 full scale) and full-scale clipping samples.
        samples[2_000] = -25_000;
        samples[2_002] = 25_000;
        samples[6_000] = i16::MAX;
        samples[6_001] = i16::MIN;

        let analysis = analyze_pcm(&samples, sample_rate, channels, None);
        assert!(!analysis.silent);
        assert!(
            analysis.underrun_glitches >= 2,
            "expected >= 2 glitches, got {}",
            analysis.underrun_glitches
        );
        assert_eq!(analysis.clipping_samples, 2);
    }

    /// Deterministic noise in -1..1 (a 64-bit LCG), so the tests don't depend on a random crate.
    fn noise(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
    }

    /// A 60 ms chirp from `f0` to `f1` Hz under a sine envelope, as stereo samples.
    fn chirp(f0: f64, f1: f64, amp: f64) -> Vec<i16> {
        let frames = 2_880usize;
        let mut out = Vec::with_capacity(frames * 2);
        let mut phase = 0.0f64;
        for n in 0..frames {
            let f = f0 + (f1 - f0) * n as f64 / frames as f64;
            phase += 2.0 * std::f64::consts::PI * f / 48_000.0;
            let env = (std::f64::consts::PI * n as f64 / frames as f64).sin();
            let v = (amp * env * phase.sin()) as i16;
            out.push(v);
            out.push(v);
        }
        out
    }

    /// Two seconds of "game audio": noise plus a chord, with `sfx` mixed in at `onset_frame` if given.
    fn scene(sfx: Option<(&[i16], usize)>) -> WavData {
        let frames = 96_000usize;
        let mut seed = 7u64;
        let mut samples = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let t = n as f64 / 48_000.0;
            let chord = 2_500.0 * ((2.0 * std::f64::consts::PI * 220.0 * t).sin() + (2.0 * std::f64::consts::PI * 277.2 * t).sin());
            let v = chord + 3_000.0 * noise(&mut seed);
            samples.push(v as i16);
            samples.push(v as i16);
        }
        if let Some((sfx, onset)) = sfx {
            for (i, s) in sfx.iter().enumerate() {
                let at = onset * 2 + i;
                samples[at] = (f64::from(samples[at]) + f64::from(*s)).clamp(-32768.0, 32767.0) as i16;
            }
        }
        WavData { sample_rate: 48_000, channels: 2, samples }
    }

    fn wav(samples: Vec<i16>) -> WavData {
        WavData { sample_rate: 48_000, channels: 2, samples }
    }

    #[test]
    fn reference_found_where_it_was_mixed_into_noisy_audio() {
        let jump = chirp(500.0, 1_800.0, 14_000.0);
        // 1234.5 ms in: frame 59,256.
        let captured = scene(Some((&jump, 59_256)));
        let found = match_reference(&captured, &wav(jump.clone()));
        assert!(found.detected, "{found:?}");
        assert!((found.timestamp_ms - 1_234.5).abs() < 0.1, "{found:?}");
        // Mixed with noise and music, the correlation is high but not perfect (it isn't the clip itself).
        assert!(found.confidence > 0.7 && found.confidence < 0.99, "{found:?}");
    }

    #[test]
    fn a_sound_that_isnt_there_isnt_matched() {
        let jump = chirp(500.0, 1_800.0, 14_000.0);
        let coin = chirp(3_000.0, 1_200.0, 14_000.0);
        // The capture has the jump; the coin sound never plays.
        let captured = scene(Some((&jump, 30_000)));
        let found = match_reference(&captured, &wav(coin));
        assert!(!found.detected, "{found:?}");
        assert!(found.confidence < 0.3, "{found:?}");
        // Nor in the same scene without any effect.
        let found = match_reference(&scene(None), &wav(jump));
        assert!(!found.detected, "{found:?}");
    }

    #[test]
    fn near_silence_never_matches() {
        // Faint dither: its shape can correlate with anything, but it's 60 dB too quiet to be the sound.
        let mut seed = 3u64;
        let faint: Vec<i16> = (0..96_000).map(|_| (2.0 * noise(&mut seed)) as i16).collect();
        let found = match_reference(&wav(faint), &wav(chirp(500.0, 1_800.0, 14_000.0)));
        assert!(!found.detected && found.confidence == 0.0, "{found:?}");
    }

    #[test]
    fn fft_correlation_matches_the_direct_sum() {
        let mut seed = 11u64;
        let x: Vec<f64> = (0..1_000).map(|_| noise(&mut seed)).collect();
        let y: Vec<f64> = (0..37).map(|_| noise(&mut seed)).collect();
        let fast = cross_correlate(&x, &y);
        assert_eq!(fast.len(), x.len() - y.len() + 1);
        for lag in [0, 1, 500, 963] {
            let direct: f64 = (0..y.len()).map(|i| x[lag + i] * y[i]).sum();
            assert!((fast[lag] - direct).abs() < 1e-9, "lag {lag}: {} vs {direct}", fast[lag]);
        }
    }

    #[test]
    fn spectrogram_rendering_produces_valid_png_and_jpeg() {
        let sample_rate = 48_000u32;
        let channels = 2u16;
        let frames = 9_600usize; // 0.2s
        let mut samples = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let t = (n as f64) / f64::from(sample_rate);
            let s = (14_000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16;
            samples.push(s);
            samples.push(s);
        }

        let png = render_spectrogram_png(&samples, sample_rate, channels).expect("render PNG");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let decoded_png = image::load_from_memory(&png).expect("decode PNG");
        assert_eq!(decoded_png.width(), 800);
        assert_eq!(decoded_png.height(), 400);

        let jpeg = render_spectrogram_jpeg(&samples, sample_rate, channels).expect("render JPEG");
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
    }
}
