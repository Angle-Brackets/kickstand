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
