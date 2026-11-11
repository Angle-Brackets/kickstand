// SPDX-License-Identifier: GPL-3.0-only
#![allow(dead_code)]
//! Screenshots strung into GIFs or MP4 clips, a PC-side rolling frame ring buffer, and modality-aware
//! visual exports (video clip, 2x3 contact sheet, or motion text summary).
//!
//! Because a single 720p RGBA frame is 3.68 MB (two frames would blow the Switch sysmodule's 8 MiB heap),
//! video is streamed frame-by-frame to the PC and buffered in PC RAM via [`VideoRingBuffer`].

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use image::codecs::gif::{GifEncoder, Repeat};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{Delay, Frame, RgbImage, RgbaImage};

use crate::client::Client;
use crate::modalities::VisualFormat;

pub const MAX_SECONDS: f64 = 30.0;
pub const MAX_FPS: u32 = 10;

/// Default maximum frames retained in the PC-side [`VideoRingBuffer`] (~10 s at 10 fps).
pub const DEFAULT_RING_MAX_FRAMES: usize = 100;
/// Default maximum byte budget in PC RAM for the [`VideoRingBuffer`] (15 MiB).
pub const DEFAULT_RING_MAX_BYTES: usize = 15 * 1024 * 1024;
/// Default rolling time window retained in [`VideoRingBuffer`] (10 seconds).
pub const DEFAULT_RING_WINDOW: Duration = Duration::from_secs(10);

/// A captured JPEG and when it was taken, from the start of the recording or stream.
#[derive(Clone, Debug)]
pub struct Shot {
    pub at: Duration,
    pub jpeg: Vec<u8>,
}

/// Rolling PC-side frame buffer holding the last ~10 seconds of streamed [`Shot`] frames in PC RAM,
/// capped by frame count (`max_frames`), memory budget (`max_bytes`), and rolling duration (`window`).
#[derive(Debug, Clone)]
pub struct VideoRingBuffer {
    frames: VecDeque<Shot>,
    total_bytes: usize,
    max_frames: usize,
    max_bytes: usize,
    window: Duration,
}

impl Default for VideoRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoRingBuffer {
    /// Creates a new PC-side rolling frame buffer with default limits
    /// (100 frames, 15 MiB, 10 s rolling window).
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_RING_MAX_FRAMES, DEFAULT_RING_MAX_BYTES, DEFAULT_RING_WINDOW)
    }

    /// Creates a rolling frame buffer with custom capacity limits.
    pub fn with_limits(max_frames: usize, max_bytes: usize, window: Duration) -> Self {
        Self {
            frames: VecDeque::with_capacity(max_frames.min(256)),
            total_bytes: 0,
            max_frames: max_frames.max(1),
            max_bytes: max_bytes.max(1024),
            window,
        }
    }

    /// Pushes a streamed [`Shot`] into the ring buffer and evicts old frames that exceed the
    /// rolling time window, frame cap, or PC RAM byte cap.
    pub fn push(&mut self, shot: Shot) {
        let newest_at = shot.at;
        self.total_bytes = self.total_bytes.saturating_add(shot.jpeg.len());
        self.frames.push_back(shot);
        self.evict(newest_at);
    }

    /// Convenience helper to push raw JPEG bytes with a capture timestamp.
    pub fn push_jpeg(&mut self, at: Duration, jpeg: Vec<u8>) {
        self.push(Shot { at, jpeg });
    }

    fn evict(&mut self, newest_at: Duration) {
        while self.frames.len() > 1 {
            let drop_for_count = self.frames.len() > self.max_frames;
            let drop_for_bytes = self.total_bytes > self.max_bytes;
            let drop_for_age = self
                .frames
                .front()
                .is_some_and(|oldest| newest_at.saturating_sub(oldest.at) > self.window);
            if !(drop_for_count || drop_for_bytes || drop_for_age) {
                break;
            }
            if let Some(removed) = self.frames.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(removed.jpeg.len());
            }
        }
    }

    /// Number of frames currently buffered in PC RAM.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Total JPEG bytes currently stored in the ring buffer.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn max_frames(&self) -> usize {
        self.max_frames
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// Clears all buffered frames.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.total_bytes = 0;
    }

    /// Access the raw underlying queue of shots without timestamp normalization.
    pub fn raw_frames(&self) -> &VecDeque<Shot> {
        &self.frames
    }

    /// Returns all buffered shots with `at` normalized so the first frame starts at `Duration::ZERO`.
    pub fn shots(&self) -> Vec<Shot> {
        let Some(first) = self.frames.front() else {
            return Vec::new();
        };
        let base = first.at;
        self.frames
            .iter()
            .map(|s| Shot {
                at: s.at.saturating_sub(base),
                jpeg: s.jpeg.clone(),
            })
            .collect()
    }

    /// Returns buffered shots from the last `duration` up to the newest frame, with `at` normalized
    /// relative to the first returned frame.
    pub fn recent(&self, duration: Duration) -> Vec<Shot> {
        let Some(newest) = self.frames.back() else {
            return Vec::new();
        };
        let cutoff = newest.at.saturating_sub(duration);
        let slice: Vec<&Shot> = self.frames.iter().filter(|s| s.at >= cutoff).collect();
        let Some(first) = slice.first() else {
            return Vec::new();
        };
        let base = first.at;
        slice
            .into_iter()
            .map(|s| Shot {
                at: s.at.saturating_sub(base),
                jpeg: s.jpeg.clone(),
            })
            .collect()
    }
}

/// Output of a modality-aware clip export ([`export_clip`] / [`record_clip`]).
#[derive(Debug, Clone)]
pub struct ClipOutput {
    /// Which visual format was produced.
    pub format: VisualFormat,
    /// Saved file path on disk (always populated for `VisualFormat::Video`, optional otherwise).
    pub path: Option<PathBuf>,
    /// Encoded binary payload:
    /// - `VisualFormat::Video`: `.mp4` (or `.gif` fallback) container bytes
    /// - `VisualFormat::ContactSheet`: 2x3 contact sheet JPEG bytes
    /// - `VisualFormat::TextSummary`: empty `Vec`
    pub bytes: Vec<u8>,
    /// MIME type of `bytes` (`"video/mp4"`, `"image/gif"`, `"image/jpeg"`, or `"text/plain"`).
    pub mime_type: &'static str,
    /// Human-readable frame timing and motion summary.
    pub summary: String,
    /// Total frames in the clip.
    pub frame_count: usize,
    /// Effective frame rate across the clip.
    pub fps: f64,
    /// Total elapsed duration of the clip.
    pub duration: Duration,
}

/// Spawns a background task that continuously streams screenshots from `client` (over USB or TCP)
/// into the PC-side `ring` buffer at up to `fps` frames per second.
pub fn spawn_background_streamer(
    client: Arc<Client>,
    ring: Arc<Mutex<VideoRingBuffer>>,
    fps: u32,
) -> tokio::task::JoinHandle<()> {
    let fps = fps.clamp(1, MAX_FPS);
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    tokio::spawn(async move {
        let start = Instant::now();
        loop {
            let tick = Instant::now();
            let at = start.elapsed();
            match client.screenshot().await {
                Ok(jpeg) => {
                    ring.lock().unwrap().push(Shot { at, jpeg });
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            tokio::time::sleep(interval.saturating_sub(tick.elapsed())).await;
        }
    })
}

/// Streams frames from the Switch (`client`) for `seconds` at up to `fps`, pushing each frame into
/// `ring` (if provided) and exporting the result according to `format`.
pub async fn record_clip(
    client: &Client,
    mut ring: Option<&mut VideoRingBuffer>,
    seconds: f64,
    fps: u32,
    width: u32,
    format: VisualFormat,
    out_path: Option<PathBuf>,
) -> Result<ClipOutput> {
    let shots = record(client, seconds, fps).await?;
    if let Some(ring) = ring.as_deref_mut() {
        let base = ring
            .raw_frames()
            .back()
            .map_or(Duration::ZERO, |last| last.at + Duration::from_millis(100));
        for shot in &shots {
            ring.push(Shot {
                at: base + shot.at,
                jpeg: shot.jpeg.clone(),
            });
        }
    }
    tokio::task::spawn_blocking(move || export_clip(&shots, width, format, out_path)).await?
}

/// Exports a slice of [`Shot`]s into the requested [`VisualFormat`]:
/// - [`VisualFormat::Video`]: encodes an `.mp4` via `ffmpeg` if available on `PATH` (or falls back to
///   a `.gif` video container if `ffmpeg` is unavailable), saves it to disk, and returns the path,
///   byte payload, and frame timing summary.
/// - [`VisualFormat::ContactSheet`]: builds a 6-frame 2x3 contact sheet JPEG (`contact_sheet(shots, 6)`),
///   optionally saving to `out_path` if given.
/// - [`VisualFormat::TextSummary`]: computes frame count, FPS, duration, and visual motion/change
///   percentages across time without image/video payloads.
pub fn export_clip(
    shots: &[Shot],
    width: u32,
    format: VisualFormat,
    out_path: Option<PathBuf>,
) -> Result<ClipOutput> {
    if shots.is_empty() {
        bail!("no frames");
    }
    let width = width.clamp(64, SCREEN.0);
    let (duration, fps) = clip_timing(shots);
    let frame_count = shots.len();

    match format {
        VisualFormat::Video => {
            let (bytes, mime_type, path) = encode_video_with_fallback(shots, width, out_path)?;
            let summary = format!(
                "saved {} ({} frames, {:.2}s, {:.1} fps, {} KB, {})",
                path.display(),
                frame_count,
                duration.as_secs_f64(),
                fps,
                bytes.len() / 1024,
                mime_type
            );
            Ok(ClipOutput {
                format,
                path: Some(path),
                bytes,
                mime_type,
                summary,
                frame_count,
                fps,
                duration,
            })
        }
        VisualFormat::ContactSheet => {
            let sheet = contact_sheet(shots, 6)?;
            if let Some(ref path) = out_path {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, &sheet)?;
            }
            let summary = match &out_path {
                Some(p) => format!(
                    "contact sheet (2x3, up to 6 frames from {} captured over {:.2}s at {:.1} fps, {} KB, saved to {})",
                    frame_count,
                    duration.as_secs_f64(),
                    fps,
                    sheet.len() / 1024,
                    p.display()
                ),
                None => format!(
                    "contact sheet (2x3, up to 6 frames from {} captured over {:.2}s at {:.1} fps, {} KB)",
                    frame_count,
                    duration.as_secs_f64(),
                    fps,
                    sheet.len() / 1024
                ),
            };
            Ok(ClipOutput {
                format,
                path: out_path,
                bytes: sheet,
                mime_type: "image/jpeg",
                summary,
                frame_count,
                fps,
                duration,
            })
        }
        VisualFormat::TextSummary => {
            let summary = motion_text_summary(shots)?;
            Ok(ClipOutput {
                format,
                path: None,
                bytes: Vec::new(),
                mime_type: "text/plain",
                summary,
                frame_count,
                fps,
                duration,
            })
        }
    }
}

fn clip_timing(shots: &[Shot]) -> (Duration, f64) {
    if shots.len() <= 1 {
        return (Duration::ZERO, 0.0);
    }
    let first = shots.first().unwrap().at;
    let last = shots.last().unwrap().at;
    let span = last.saturating_sub(first);
    let secs = span.as_secs_f64();
    let fps = if secs > 0.0 {
        (shots.len() - 1) as f64 / secs
    } else {
        0.0
    };
    (span, fps)
}

/// Computes a detailed text summary of frame count, FPS, duration, and visual motion/change
/// percentages across time.
pub fn motion_text_summary(shots: &[Shot]) -> Result<String> {
    if shots.is_empty() {
        bail!("no frames");
    }
    let (duration, fps) = clip_timing(shots);
    let thumbs: Vec<image::GrayImage> = shots
        .iter()
        .map(|s| thumbnail(&s.jpeg, None))
        .collect::<Result<_>>()?;

    let base_at = shots[0].at;
    let mut step_changes = Vec::with_capacity(shots.len().saturating_sub(1));
    let mut timeline = String::new();

    for i in 1..shots.len() {
        let pct = changed(&thumbs[i - 1], &thumbs[i]) * 100.0;
        step_changes.push(pct);
        let t_prev = shots[i - 1].at.saturating_sub(base_at).as_secs_f64();
        let t_curr = shots[i].at.saturating_sub(base_at).as_secs_f64();
        let state = if pct > CHANGED * 100.0 {
            "motion"
        } else if pct < STILL * 100.0 {
            "still"
        } else {
            "minor"
        };
        let _ = writeln!(
            timeline,
            "  frame {:02} -> {:02} ({t_prev:.2}s..{t_curr:.2}s): {pct:.1}% changed ({state})",
            i - 1,
            i
        );
    }

    let overall_change = if thumbs.len() >= 2 {
        changed(thumbs.first().unwrap(), thumbs.last().unwrap()) * 100.0
    } else {
        0.0
    };
    let mean_change = if step_changes.is_empty() {
        0.0
    } else {
        step_changes.iter().sum::<f32>() / step_changes.len() as f32
    };
    let peak_change = step_changes.iter().copied().fold(0.0_f32, f32::max);

    let mut out = format!(
        "visual summary: {} frames, {:.2}s duration, {:.1} fps, motion: mean {:.1}%, peak {:.1}%, net start->end {:.1}%",
        shots.len(),
        duration.as_secs_f64(),
        fps,
        mean_change,
        peak_change,
        overall_change
    );
    if !timeline.is_empty() {
        out.push('\n');
        out.push_str(timeline.trim_end());
    }
    Ok(out)
}

fn default_video_path(ext: &str) -> Result<PathBuf> {
    let base = std::env::var_os("HOME")
        .map(|h| Path::new(&h).join("Pictures/kickstand"))
        .unwrap_or_else(|| std::env::temp_dir().join("kickstand"));
    std::fs::create_dir_all(&base)?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    Ok(base.join(format!("clip-{nanos}.{ext}")))
}

/// Checks if `ffmpeg` is runnable on `PATH`.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Attempts to encode `shots` as an `.mp4` via `ffmpeg`; falls back to `.gif` container if `ffmpeg`
/// is unavailable or fails.
fn encode_video_with_fallback(
    shots: &[Shot],
    width: u32,
    out_path: Option<PathBuf>,
) -> Result<(Vec<u8>, &'static str, PathBuf)> {
    if ffmpeg_available() {
        let mp4_path = match &out_path {
            Some(p) => p.with_extension("mp4"),
            None => default_video_path("mp4")?,
        };
        if let Ok(bytes) = encode_mp4_ffmpeg(shots, width, &mp4_path) {
            return Ok((bytes, "video/mp4", mp4_path));
        }
    }

    // Fallback to GIF container when ffmpeg is not installed or mp4 encoding failed.
    let gif_bytes = gif(shots, width)?;
    let gif_path = match out_path {
        Some(p) => p,
        None => default_video_path("gif")?,
    };
    if let Some(parent) = gif_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&gif_path, &gif_bytes)
        .with_context(|| format!("writing video fallback to {}", gif_path.display()))?;
    Ok((gif_bytes, "image/gif", gif_path))
}

fn encode_mp4_ffmpeg(shots: &[Shot], width: u32, dest: &Path) -> Result<Vec<u8>> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let work_dir = std::env::temp_dir().join(format!("kickstand-ffmpeg-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&work_dir)?;

    let result = (|| -> Result<Vec<u8>> {
        let mut concat_list = String::new();
        for (i, shot) in shots.iter().enumerate() {
            let scaled = reframe(&shot.jpeg, None, Some(width))?;
            let frame_path = work_dir.join(format!("frame_{i:04}.jpg"));
            std::fs::write(&frame_path, &scaled)?;
            let dur = match shots.get(i + 1) {
                Some(next) => next.at.saturating_sub(shot.at),
                None => shots
                    .get(1)
                    .map_or(Duration::from_millis(200), |s| s.at.saturating_sub(shots[0].at)),
            };
            let secs = dur.as_secs_f64().max(0.02);
            let _ = writeln!(concat_list, "file '{}'", frame_path.display());
            let _ = writeln!(concat_list, "duration {secs:.4}");
        }
        if let Some((last_idx, _)) = shots.iter().enumerate().last() {
            let last_path = work_dir.join(format!("frame_{last_idx:04}.jpg"));
            let _ = writeln!(concat_list, "file '{}'", last_path.display());
        }

        let concat_path = work_dir.join("concat.txt");
        std::fs::write(&concat_path, &concat_list)?;

        if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        // Try H.264 first, then mpeg4 fallback if libx264 is absent.
        for codec in ["libx264", "mpeg4"] {
            let status = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-loglevel",
                    "error",
                    "-f",
                    "concat",
                    "-safe",
                    "0",
                    "-i",
                ])
                .arg(&concat_path)
                .args([
                    "-vf",
                    "pad=ceil(iw/2)*2:ceil(ih/2)*2",
                    "-c:v",
                    codec,
                    "-pix_fmt",
                    "yuv420p",
                    "-movflags",
                    "+faststart",
                ])
                .arg(dest)
                .status();

            if status.is_ok_and(|s| s.success()) && dest.exists() {
                let data = std::fs::read(dest)?;
                if !data.is_empty() {
                    return Ok(data);
                }
            }
        }
        bail!("ffmpeg failed to encode mp4")
    })();

    let _ = std::fs::remove_dir_all(&work_dir);
    result
}

/// Screenshots for `seconds`, at most `fps` a second (fewer if capturing and transferring is slower).
pub async fn record(client: &Client, seconds: f64, fps: u32) -> Result<Vec<Shot>> {
    if !(0.0..=MAX_SECONDS).contains(&seconds) || fps == 0 || fps > MAX_FPS {
        bail!("seconds must be 0-{MAX_SECONDS} and fps 1-{MAX_FPS}");
    }
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let length = Duration::from_secs_f64(seconds);
    let start = Instant::now();
    let mut shots = Vec::new();
    loop {
        let at = start.elapsed();
        shots.push(Shot { at, jpeg: client.screenshot().await? });
        let next = at + interval;
        if next > length {
            break;
        }
        tokio::time::sleep(next.saturating_sub(start.elapsed())).await;
    }
    Ok(shots)
}

fn decode(shot: &Shot, width: u32) -> Result<RgbImage> {
    let img = image::load_from_memory_with_format(&shot.jpeg, image::ImageFormat::Jpeg)?.to_rgb8();
    let height = (u64::from(img.height()) * u64::from(width) / u64::from(img.width().max(1))).max(1) as u32;
    Ok(image::imageops::resize(&img, width, height, FilterType::Triangle))
}

/// An endlessly looping GIF, `width` pixels wide, each frame shown until the next was captured.
pub fn gif(shots: &[Shot], width: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new_with_speed(&mut out, 10);
        enc.set_repeat(Repeat::Infinite)?;
        for (i, shot) in shots.iter().enumerate() {
            let shown = match shots.get(i + 1) {
                Some(next) => next.at.saturating_sub(shot.at),
                None => shots.get(1).map_or(Duration::from_secs(1), |s| s.at.saturating_sub(shots[0].at)),
            };
            let rgba: RgbaImage = image::DynamicImage::ImageRgb8(decode(shot, width)?).to_rgba8();
            enc.encode_frame(Frame::from_parts(rgba, 0, 0, Delay::from_saturating_duration(shown)))?;
        }
    }
    Ok(out)
}

/// Up to `count` evenly spaced frames in a grid three wide, 426 px each, as a JPEG.
pub fn contact_sheet(shots: &[Shot], count: usize) -> Result<Vec<u8>> {
    const CELL_W: u32 = 426;
    let picks: Vec<&Shot> = match shots.len() {
        0 => bail!("no frames"),
        n if n <= count => shots.iter().collect(),
        n => (0..count).map(|i| &shots[i * (n - 1) / (count - 1)]).collect(),
    };
    let cells: Vec<RgbImage> = picks.iter().map(|s| decode(s, CELL_W)).collect::<Result<_>>()?;
    let cell_h = cells[0].height();
    let cols = cells.len().min(3) as u32;
    let rows = cells.len().div_ceil(3) as u32;
    let mut sheet = RgbImage::new(cols * CELL_W, rows * cell_h);
    for (i, cell) in cells.iter().enumerate() {
        let (x, y) = ((i as u32 % 3) * CELL_W, (i as u32 / 3) * cell_h);
        image::imageops::replace(&mut sheet, cell, i64::from(x), i64::from(y));
    }
    let mut out = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut out, 80).encode_image(&sheet)?;
    Ok(out.into_inner())
}

/// Crops `jpeg` to `region` (x, y, w, h in the 1280x720 screen) and scales it to `width` pixels wide; the
/// original if neither is asked for.
pub fn reframe(jpeg: &[u8], region: Option<Region>, width: Option<u32>) -> Result<Vec<u8>> {
    frame(jpeg, region, width, false)
}

/// [`reframe`], with a coordinate grid drawn over it first if `grid`.
pub fn frame(jpeg: &[u8], region: Option<Region>, width: Option<u32>, grid: bool) -> Result<Vec<u8>> {
    if region.is_none() && width.is_none() && !grid {
        return Ok(jpeg.to_vec());
    }
    let mut img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.to_rgb8();
    if grid {
        draw_grid(&mut img, region.unwrap_or((0, 0, SCREEN.0, SCREEN.1)));
    }
    if let Some(region) = region {
        img = crop(&img, region)?;
    }
    if let Some(width) = width.filter(|&w| w < img.width()) {
        let height = (u64::from(img.height()) * u64::from(width) / u64::from(img.width())).max(1) as u32;
        img = image::imageops::resize(&img, width, height, FilterType::Triangle);
    }
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img)?;
    Ok(out)
}

/// x, y, width, height in the 1280x720 screen.
pub type Region = (u32, u32, u32, u32);

/// `region` of `img`, clipped to the image; an error if none of it is on the screen.
fn crop<P: image::Pixel<Subpixel = u8> + 'static>(img: &image::ImageBuffer<P, Vec<u8>>, (x, y, w, h): Region) -> Result<image::ImageBuffer<P, Vec<u8>>> {
    let (iw, ih) = img.dimensions();
    if x >= iw || y >= ih || w == 0 || h == 0 {
        bail!("region {x},{y},{w},{h} is off the {iw}x{ih} screen");
    }
    Ok(image::imageops::crop_imm(img, x, y, w.min(iw - x), h.min(ih - y)).to_image())
}

pub const SCREEN: (u32, u32) = (1280, 720);

/// Grid lines every this many screen pixels.
const GRID: u32 = 100;

/// Lines every [`GRID`] pixels, labelled with their screen coordinate along the top and left of `area`
/// (the part of the screen that will be shown), so positions can be read off instead of guessed.
fn draw_grid(img: &mut RgbImage, area: Region) {
    let (w, h) = img.dimensions();
    let tint = |p: &mut image::Rgb<u8>| {
        let [r, g, b] = p.0;
        p.0 = [((u16::from(r) + 0xFF) / 2) as u8, ((u16::from(g) + 0x45) / 2) as u8, ((u16::from(b) + 0x54) / 2) as u8];
    };
    for x in (GRID..w).step_by(GRID as usize) {
        for y in 0..h {
            tint(img.get_pixel_mut(x, y));
        }
    }
    for y in (GRID..h).step_by(GRID as usize) {
        for x in 0..w {
            tint(img.get_pixel_mut(x, y));
        }
    }
    let (ax, ay, aw, ah) = area;
    let first = |from: u32| from.div_ceil(GRID).max(1) * GRID;
    for x in (first(ax)..(ax + aw).min(w)).step_by(GRID as usize) {
        label(img, x + 3, ay + 3, x);
    }
    for y in (first(ay)..(ay + ah).min(h)).step_by(GRID as usize) {
        label(img, ax + 3, y + 3, y);
    }
}

/// `value` in a 3x5 pixel font at 4x, white on a dark box, with its top-left at (x, y).
fn label(img: &mut RgbImage, x: u32, y: u32, value: u32) {
    const DIGITS: [[u8; 5]; 10] = [
        [7, 5, 5, 5, 7],
        [2, 6, 2, 2, 7],
        [7, 1, 7, 4, 7],
        [7, 1, 7, 1, 7],
        [5, 5, 7, 1, 1],
        [7, 4, 7, 1, 7],
        [7, 4, 7, 5, 7],
        [7, 1, 1, 1, 1],
        [7, 5, 7, 5, 7],
        [7, 5, 7, 1, 7],
    ];
    const S: u32 = 4;
    let text = value.to_string();
    let (bw, bh) = (text.len() as u32 * 4 * S + S, 5 * S + 2 * S);
    let (w, h) = img.dimensions();
    for py in y..(y + bh).min(h) {
        for px in x..(x + bw).min(w) {
            img.put_pixel(px, py, image::Rgb([0x15, 0x17, 0x1C]));
        }
    }
    for (i, ch) in text.bytes().enumerate() {
        let glyph = DIGITS[usize::from(ch - b'0')];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3 {
                if bits & (4 >> col) == 0 {
                    continue;
                }
                for dy in 0..S {
                    for dx in 0..S {
                        let (px, py) = (x + S + (i as u32 * 4 + col) * S + dx, y + S + row as u32 * S + dy);
                        if px < w && py < h {
                            img.put_pixel(px, py, image::Rgb([0xF2, 0xF0, 0xEA]));
                        }
                    }
                }
            }
        }
    }
}

/// What [`wait`] waits for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Until {
    /// The screen differs from when the wait began (a load finished, a dialog opened).
    Change,
    /// Two captures in a row look the same as the one before (a transition or animation ended).
    Still,
}

pub struct Waited {
    /// The last capture.
    pub jpeg: Vec<u8>,
    /// False: the timeout came first.
    pub met: bool,
    pub elapsed: Duration,
    pub captures: usize,
}

pub const MAX_WAIT_SECONDS: f64 = 60.0;

/// Captures until `until` holds or `timeout` passes, comparing small grayscale copies of `region` (the
/// whole screen if `None`) so JPEG noise and the odd pixel don't count.
pub async fn wait(client: &Client, until: Until, region: Option<Region>, timeout: Duration) -> Result<Waited> {
    let start = Instant::now();
    let first = client.screenshot().await?;
    let reference = thumbnail(&first, region)?;
    let mut previous = reference.clone();
    let (mut jpeg, mut captures, mut still_for) = (first, 1, 0);
    loop {
        if start.elapsed() >= timeout {
            return Ok(Waited { jpeg, met: false, elapsed: start.elapsed(), captures });
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        jpeg = client.screenshot().await?;
        captures += 1;
        let now = thumbnail(&jpeg, region)?;
        let met = match until {
            Until::Change => changed(&reference, &now) > CHANGED,
            Until::Still => {
                still_for = if changed(&previous, &now) < STILL { still_for + 1 } else { 0 };
                still_for >= 2
            }
        };
        if met {
            return Ok(Waited { jpeg, met, elapsed: start.elapsed(), captures });
        }
        previous = now;
    }
}

/// Over this fraction of thumbnail pixels changed: the screen changed.
pub const CHANGED: f32 = 0.01;
/// Under this: nothing moved.
pub const STILL: f32 = 0.003;

/// A 128x72 (for the whole screen) grayscale copy.
pub fn thumbnail(jpeg: &[u8], region: Option<Region>) -> Result<image::GrayImage> {
    let mut img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.to_luma8();
    if let Some(region) = region {
        img = crop(&img, region)?;
    }
    let (w, h) = ((img.width() / 10).max(1), (img.height() / 10).max(1));
    Ok(image::imageops::resize(&img, w, h, FilterType::Triangle))
}

/// The fraction of pixels more than 16 levels apart.
pub fn changed(a: &image::GrayImage, b: &image::GrayImage) -> f32 {
    let differ = a.as_raw().iter().zip(b.as_raw()).filter(|(x, y)| x.abs_diff(**y) > 16).count();
    differ as f32 / a.as_raw().len().max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modalities::{Modalities, Modality};

    fn test_jpeg(r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = RgbImage::from_pixel(160, 90, image::Rgb([r, g, b]));
        let mut out = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut out, 80).encode_image(&img).unwrap();
        out.into_inner()
    }

    fn sample_shots() -> Vec<Shot> {
        vec![
            Shot { at: Duration::from_millis(0), jpeg: test_jpeg(10, 10, 10) },
            Shot { at: Duration::from_millis(200), jpeg: test_jpeg(12, 12, 12) },
            Shot { at: Duration::from_millis(400), jpeg: test_jpeg(220, 40, 40) },
            Shot { at: Duration::from_millis(600), jpeg: test_jpeg(220, 40, 40) },
            Shot { at: Duration::from_millis(800), jpeg: test_jpeg(30, 200, 50) },
            Shot { at: Duration::from_millis(1000), jpeg: test_jpeg(30, 200, 50) },
        ]
    }

    #[test]
    fn ring_buffer_respects_frame_byte_and_window_caps() {
        let jpg = test_jpeg(100, 150, 200);
        let frame_size = jpg.len();

        // Cap at 4 frames and 10 seconds
        let mut ring = VideoRingBuffer::with_limits(4, frame_size * 10, Duration::from_secs(10));
        for i in 0..6 {
            ring.push_jpeg(Duration::from_secs(i), jpg.clone());
        }
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.total_bytes(), frame_size * 4);
        let shots = ring.shots();
        assert_eq!(shots[0].at, Duration::ZERO);
        assert_eq!(shots[3].at, Duration::from_secs(3));

        // Window eviction (window = 2s)
        let mut window_ring = VideoRingBuffer::with_limits(100, frame_size * 100, Duration::from_secs(2));
        window_ring.push_jpeg(Duration::from_secs(0), jpg.clone());
        window_ring.push_jpeg(Duration::from_secs(1), jpg.clone());
        window_ring.push_jpeg(Duration::from_secs(2), jpg.clone());
        window_ring.push_jpeg(Duration::from_secs(3), jpg.clone());
        // Frame at 0s is 3s older than newest (3s) > 2s window, so evicted
        assert_eq!(window_ring.len(), 3);
        assert_eq!(window_ring.raw_frames().front().unwrap().at, Duration::from_secs(1));

        // Byte cap eviction (allow at most 2 frames worth of bytes)
        let mut byte_ring = VideoRingBuffer::with_limits(100, frame_size * 2, Duration::from_secs(60));
        byte_ring.push_jpeg(Duration::from_millis(100), jpg.clone());
        byte_ring.push_jpeg(Duration::from_millis(200), jpg.clone());
        byte_ring.push_jpeg(Duration::from_millis(300), jpg.clone());
        assert_eq!(byte_ring.len(), 2);
        assert!(byte_ring.total_bytes() <= frame_size * 2);
    }

    #[test]
    fn modality_aware_formatting_text_contact_sheet_and_video() {
        let shots = sample_shots();

        // 1. Text-only modality -> VisualFormat::TextSummary
        let text_fmt = Modalities::NONE.preferred_visual();
        assert_eq!(text_fmt, VisualFormat::TextSummary);
        let text_out = export_clip(&shots, 320, text_fmt, None).unwrap();
        assert_eq!(text_out.format, VisualFormat::TextSummary);
        assert!(text_out.bytes.is_empty());
        assert_eq!(text_out.frame_count, 6);
        assert!(text_out.summary.contains("6 frames"));
        assert!(text_out.summary.contains("1.00s duration"));
        assert!(text_out.summary.contains("5.0 fps"));
        assert!(text_out.summary.contains("(motion)"));

        // 2. Image modality -> VisualFormat::ContactSheet
        let img_fmt = Modalities::from(&[Modality::Image][..]).preferred_visual();
        assert_eq!(img_fmt, VisualFormat::ContactSheet);
        let sheet_out = export_clip(&shots, 320, img_fmt, None).unwrap();
        assert_eq!(sheet_out.format, VisualFormat::ContactSheet);
        assert_eq!(sheet_out.mime_type, "image/jpeg");
        assert!(!sheet_out.bytes.is_empty());
        let decoded = image::load_from_memory_with_format(&sheet_out.bytes, image::ImageFormat::Jpeg).unwrap();
        assert_eq!(decoded.width(), 426 * 3);

        // 3. Video + Image modality -> VisualFormat::Video (VIDEO prioritized over IMAGE)
        let vid_fmt = Modalities::from(&[Modality::Image, Modality::Video][..]).preferred_visual();
        assert_eq!(vid_fmt, VisualFormat::Video);
        let tmp_out = std::env::temp_dir().join(format!("kickstand-test-video-{}.mp4", std::process::id()));
        let vid_out = export_clip(&shots, 320, vid_fmt, Some(tmp_out)).unwrap();
        assert_eq!(vid_out.format, VisualFormat::Video);
        assert!(!vid_out.bytes.is_empty());
        assert!(vid_out. mime_type == "video/mp4" || vid_out.mime_type == "image/gif");
        let saved_path = vid_out.path.unwrap();
        assert!(saved_path.exists());
        let _ = std::fs::remove_file(saved_path);
    }
}
