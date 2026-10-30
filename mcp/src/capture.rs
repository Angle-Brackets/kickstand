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
