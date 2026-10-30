// SPDX-License-Identifier: GPL-3.0-only
//! Screenshots strung into GIFs, plus a contact sheet of frames: what an agent can actually look at, since
//! image viewers in agents show a GIF's first frame only.

use std::io::Cursor;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use image::codecs::gif::{GifEncoder, Repeat};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{Delay, Frame, RgbImage, RgbaImage};

use crate::client::Client;

pub const MAX_SECONDS: f64 = 30.0;
pub const MAX_FPS: u32 = 10;

/// A captured JPEG and when it was taken, from the start of the recording.
pub struct Shot {
    pub at: Duration,
    pub jpeg: Vec<u8>,
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
    let height = (u64::from(img.height()) * u64::from(width) / u64::from(img.width())) as u32;
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
                Some(next) => next.at - shot.at,
                None => shots.get(1).map_or(Duration::from_secs(1), |s| s.at - shots[0].at),
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
pub fn reframe(jpeg: &[u8], region: Option<(u32, u32, u32, u32)>, width: Option<u32>) -> Result<Vec<u8>> {
    if region.is_none() && width.is_none() {
        return Ok(jpeg.to_vec());
    }
    let mut img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.to_rgb8();
    if let Some((x, y, w, h)) = region {
        let (iw, ih) = img.dimensions();
        if x >= iw || y >= ih || w == 0 || h == 0 {
            anyhow::bail!("region {x},{y},{w},{h} is off the {iw}x{ih} screen");
        }
        img = image::imageops::crop_imm(&img, x, y, w.min(iw - x), h.min(ih - y)).to_image();
    }
    if let Some(width) = width.filter(|&w| w < img.width()) {
        let height = (u64::from(img.height()) * u64::from(width) / u64::from(img.width())).max(1) as u32;
        img = image::imageops::resize(&img, width, height, FilterType::Triangle);
    }
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img)?;
    Ok(out)
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
pub async fn wait(client: &Client, until: Until, region: Option<(u32, u32, u32, u32)>, timeout: Duration) -> Result<Waited> {
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
const CHANGED: f32 = 0.01;
/// Under this: nothing moved.
const STILL: f32 = 0.003;

/// A 128x72 (for the whole screen) grayscale copy.
fn thumbnail(jpeg: &[u8], region: Option<(u32, u32, u32, u32)>) -> Result<image::GrayImage> {
    let mut img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.to_luma8();
    if let Some((x, y, w, h)) = region {
        let (iw, ih) = img.dimensions();
        if x >= iw || y >= ih || w == 0 || h == 0 {
            bail!("region {x},{y},{w},{h} is off the {iw}x{ih} screen");
        }
        img = image::imageops::crop_imm(&img, x, y, w.min(iw - x), h.min(ih - y)).to_image();
    }
    let (w, h) = ((img.width() / 10).max(1), (img.height() / 10).max(1));
    Ok(image::imageops::resize(&img, w, h, FilterType::Triangle))
}

/// The fraction of pixels more than 16 levels apart.
fn changed(a: &image::GrayImage, b: &image::GrayImage) -> f32 {
    let differ = a.as_raw().iter().zip(b.as_raw()).filter(|(x, y)| x.abs_diff(**y) > 16).count();
    differ as f32 / a.as_raw().len().max(1) as f32
}
