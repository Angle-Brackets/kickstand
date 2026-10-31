// SPDX-License-Identifier: GPL-3.0-only
//! `input_until`: input played from this PC while it watches the screen, stopped as soon as a condition
//! holds. The agent decides what to watch (from a screenshot it has seen); this only compares pixels, a
//! few times a second, so a menu walk or "hold right until the door" is one tool call.
//!
//! Frames come over a second connection, so captures don't wait behind input. Holding goes through
//! [`proto::Request::Hold`] under a short lease that's renewed while the loop runs, so nothing stays held
//! if this PC goes away.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use image::GrayImage;
use image::imageops::FilterType;
use proto::{Button, InputStep};

use crate::capture::{self, Region};
use crate::client::Client;

/// What to do while watching.
#[derive(Clone, Debug)]
pub enum Action {
    /// Hold these buttons, sticks or touch the whole time.
    Hold(InputStep),
    /// Press these together every `every`, each held `hold_ms`.
    Repeat { buttons: Vec<Button>, every: Duration, hold_ms: u32 },
    /// Play these steps over and over.
    Steps(Vec<InputStep>),
}

/// When to stop.
pub enum Until {
    /// The region (or the screen) differs from when the loop began.
    Change(Option<Region>),
    /// The region (or the screen) stops changing.
    Still(Option<Region>),
    /// `patch` shows up anywhere in `search`.
    Appears { patch: Patch, search: Region },
    /// `patch` is no longer anywhere in `search`.
    Gone { patch: Patch, search: Region },
}

pub struct Outcome {
    /// False: the timeout came first.
    pub met: bool,
    pub elapsed: Duration,
    pub frames: usize,
    /// The frame that met the condition (or the last one).
    pub jpeg: Vec<u8>,
    /// For Appears and Gone: where the patch matched best in the last frame (top-left, screen pixels), and
    /// its score there (under [`MATCH`] is a match).
    pub best: Option<((u32, u32), f32)>,
}

pub const MAX_SECONDS: f64 = 60.0;

/// Matching works on copies scaled down this much: 320x180 for the screen.
const SCALE: u32 = 4;
/// Scores are the mean gray-level difference over the patch's own contrast, so a patch that's mostly
/// background (a line of text) isn't found in plain background: there it scores about 1. Under this a
/// patch matches. On the home menu (2026-10-30) a title scored 0 against itself, 0.73-0.77 against other
/// titles in the same place and 0.8-0.92 once gone; cover art kept 0.14-0.18 with its selection border
/// changed.
pub const MATCH: f32 = 0.35;
/// And over this it's gone (a gap between the two, so a flickering edge doesn't decide).
const GONE: f32 = 0.6;
/// Patches with less contrast than this (mean distance from their average gray) are refused as flat.
const FLAT: f32 = 6.0;
/// How long one Hold lease lasts, and how often it's renewed while the loop runs.
const LEASE_MS: u32 = 1_500;
const RENEW: Duration = Duration::from_millis(500);

/// A piece of an earlier screenshot to look for.
pub struct Patch {
    pixels: GrayImage,
    /// Mean distance of its pixels from their average: how much there is to recognise.
    contrast: f32,
}

impl Patch {
    pub fn cut(jpeg: &[u8], at: Region) -> Result<Patch> {
        let (x, y, w, h) = at;
        if w < SCALE * 2 || h < SCALE * 2 {
            bail!("the region to look for must be at least {0}x{0} pixels", SCALE * 2);
        }
        let screen = small(jpeg)?;
        let (sw, sh) = screen.dimensions();
        let (px, py) = (x / SCALE, y / SCALE);
        if px >= sw || py >= sh {
            bail!("region {x},{y},{w},{h} is off the screen");
        }
        let (pw, ph) = ((w / SCALE).min(sw - px), (h / SCALE).min(sh - py));
        let pixels = image::imageops::crop_imm(&screen, px, py, pw, ph).to_image();
        let mean = pixels.pixels().map(|p| f32::from(p.0[0])).sum::<f32>() / (pw * ph) as f32;
        let contrast = pixels.pixels().map(|p| (f32::from(p.0[0]) - mean).abs()).sum::<f32>() / (pw * ph) as f32;
        if contrast < FLAT {
            bail!("region {x},{y},{w},{h} is nearly one flat colour: pick something with detail (text, an icon, an edge)");
        }
        Ok(Patch { pixels, contrast })
    }
}

/// Where to look for a patch by default: around where it was, 96 pixels each way.
pub fn around((x, y, w, h): Region) -> Region {
    let (x0, y0) = (x.saturating_sub(96), y.saturating_sub(96));
    let (x1, y1) = ((x + w + 96).min(capture::SCREEN.0), (y + h + 96).min(capture::SCREEN.1));
    (x0, y0, x1 - x0, y1 - y0)
}

/// Plays `action` while watching the screen, until `until` holds or `timeout` passes; everything held is
/// released before it returns.
pub async fn run(client: Arc<Client>, action: Action, until: Until, timeout: Duration) -> Result<Outcome> {
    let frames = client.another();
    let start = Instant::now();
    let first = frames.screenshot().await?;
    let stop = Arc::new(AtomicBool::new(false));
    let acting = tokio::spawn(act(client.clone(), action, stop.clone()));

    let reference = match &until {
        Until::Change(region) | Until::Still(region) => Some(capture::thumbnail(&first, *region)?),
        _ => None,
    };
    let mut previous = reference.clone();
    let (mut jpeg, mut count, mut still_for) = (first, 1, 0);
    let mut best = None;
    let result = loop {
        // A condition already true at the start (the patch is on screen, say) ends it at once.
        let met = match &until {
            Until::Change(region) => {
                let now = capture::thumbnail(&jpeg, *region)?;
                count > 1 && reference.as_ref().is_some_and(|r| capture::changed(r, &now) > capture::CHANGED)
            }
            Until::Still(region) => {
                let now = capture::thumbnail(&jpeg, *region)?;
                if count > 1 {
                    still_for = if previous.as_ref().is_some_and(|p| capture::changed(p, &now) < capture::STILL) { still_for + 1 } else { 0 };
                }
                previous = Some(now);
                still_for >= 2
            }
            Until::Appears { patch, search } | Until::Gone { patch, search } => {
                let (at, score) = find(&small(&jpeg)?, patch, *search);
                best = Some((at, score));
                if matches!(until, Until::Appears { .. }) { score < MATCH } else { score > GONE }
            }
        };
        if met {
            break Ok(true);
        }
        // The action only ends on its own by failing.
        if acting.is_finished() || start.elapsed() >= timeout {
            break Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        match frames.screenshot().await {
            Ok(next) => jpeg = next,
            Err(e) => break Err(e),
        }
        count += 1;
    };
    stop.store(true, Ordering::Relaxed);
    let acted = tokio::time::timeout(Duration::from_secs(25), acting).await;
    frames.disconnect().await;
    let met = result?;
    match acted {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => return Err(e),
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => bail!("the input didn't stop within 25 s"),
    }
    Ok(Outcome { met, elapsed: start.elapsed(), frames: count, jpeg, best })
}

/// Plays `action` until `stop`, then releases everything.
async fn act(client: Arc<Client>, action: Action, stop: Arc<AtomicBool>) -> Result<()> {
    let result = async {
        match &action {
            Action::Hold(step) => {
                while !stop.load(Ordering::Relaxed) {
                    if !client.hold(InputStep { ms: LEASE_MS, ..step.clone() }).await? {
                        bail!("Kickstand on the console predates holding buttons (protocol 23): update it, or use steps");
                    }
                    pause(RENEW, &stop).await;
                }
            }
            Action::Repeat { buttons, every, hold_ms } => {
                // Keeps the controller attached, so each press skips the second a new one needs to settle.
                let kept = client.hold(InputStep { ms: LEASE_MS * 2, ..Default::default() }).await?;
                while !stop.load(Ordering::Relaxed) {
                    let began = Instant::now();
                    client.press(buttons.clone(), 1, *hold_ms).await?;
                    if kept {
                        client.hold(InputStep { ms: LEASE_MS * 2, ..Default::default() }).await?;
                    }
                    pause(every.saturating_sub(began.elapsed()), &stop).await;
                }
            }
            Action::Steps(steps) => {
                let length: u32 = steps.iter().map(|s| s.ms).sum();
                while !stop.load(Ordering::Relaxed) {
                    let lease = (length + LEASE_MS).min(proto::MAX_HOLD_LEASE_MS);
                    client.hold(InputStep { ms: lease, ..Default::default() }).await?;
                    client.input(steps.clone()).await?;
                }
            }
        }
        anyhow::Ok(())
    }
    .await;
    // Releases and detaches now rather than when the lease runs out.
    let _ = client.hold(InputStep::default()).await;
    result
}

/// Sleeps up to `length`, waking early once `stop` is set.
async fn pause(length: Duration, stop: &AtomicBool) {
    let end = Instant::now() + length;
    while !stop.load(Ordering::Relaxed) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(25).min(end.saturating_duration_since(Instant::now()))).await;
    }
}

/// The screen in grayscale at 1/[`SCALE`] size.
fn small(jpeg: &[u8]) -> Result<GrayImage> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.to_luma8();
    Ok(image::imageops::resize(&img, img.width() / SCALE, img.height() / SCALE, FilterType::Triangle))
}

/// Where `patch` fits best inside `search` (screen pixels), and its score there (see [`MATCH`]).
fn find(screen: &GrayImage, patch: &Patch, search: Region) -> ((u32, u32), f32) {
    let (sw, sh) = screen.dimensions();
    let (pw, ph) = patch.pixels.dimensions();
    let (x0, y0) = ((search.0 / SCALE).min(sw), (search.1 / SCALE).min(sh));
    let x1 = ((search.0 + search.2) / SCALE).min(sw).saturating_sub(pw);
    let y1 = ((search.1 + search.3) / SCALE).min(sh).saturating_sub(ph);
    let area = (pw * ph) as u64;
    let mut best = (u64::MAX, (x0, y0));
    for y in y0..=y1.max(y0) {
        for x in x0..=x1.max(x0) {
            if x + pw > sw || y + ph > sh {
                continue;
            }
            let mut sum = 0u64;
            'rows: for py in 0..ph {
                for px in 0..pw {
                    let a = screen.get_pixel(x + px, y + py).0[0];
                    let b = patch.pixels.get_pixel(px, py).0[0];
                    sum += u64::from(a.abs_diff(b));
                }
                if sum >= best.0 {
                    break 'rows;
                }
            }
            if sum < best.0 {
                best = (sum, (x, y));
            }
        }
    }
    let score = if best.0 == u64::MAX { f32::MAX } else { best.0 as f32 / area as f32 / patch.contrast };
    ((best.1.0 * SCALE, best.1.1 * SCALE), score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg_of(img: &image::RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(img).unwrap();
        out
    }

    /// A gray screen with a bright 120x40 box with a dark stripe at (x, y).
    fn screen_with_box(x: u32, y: u32) -> Vec<u8> {
        let mut img = image::RgbImage::from_pixel(1280, 720, image::Rgb([60, 60, 70]));
        for py in y..y + 40 {
            for px in x..x + 120 {
                let v = if (py - y) / 10 == 2 { 20 } else { 230 };
                img.put_pixel(px, py, image::Rgb([v, v, v]));
            }
        }
        jpeg_of(&img)
    }

    #[test]
    fn finds_a_patch_where_it_moved_and_not_when_gone() {
        let before = screen_with_box(400, 300);
        let cut = (400, 300, 120, 40);
        let patch = Patch::cut(&before, cut).unwrap();
        let (at, score) = find(&small(&before).unwrap(), &patch, around(cut));
        assert_eq!(at, (400, 300));
        assert!(score < 0.1, "{score}");

        let moved = screen_with_box(460, 340);
        let (at, score) = find(&small(&moved).unwrap(), &patch, around(cut));
        assert_eq!(at, (460, 340));
        assert!(score < MATCH, "{score}");

        let gone = jpeg_of(&image::RgbImage::from_pixel(1280, 720, image::Rgb([60, 60, 70])));
        let (_, score) = find(&small(&gone).unwrap(), &patch, around(cut));
        assert!(score > GONE, "{score}");
    }
}
