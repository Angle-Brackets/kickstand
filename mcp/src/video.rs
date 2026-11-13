// SPDX-License-Identifier: GPL-3.0-only
//! Gameplay video from the console's recorder (`grc:d`, protocol 30): H.264 frames streamed by
//! [`crate::client::Client::record_video`], muxed into an `.mp4` with `ffmpeg -c copy` (no re-encode) and
//! decoded into [`Shot`]s for GIFs and contact sheets. Only works where audio capture does (a game that
//! records gameplay, or homebrew in such a game's place); otherwise callers fall back to screenshots.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::capture::Shot;

/// How long [`crate::client::Client::record_video`] waits for the recorder's first keyframe.
pub const KEYFRAME_WAIT: Duration = Duration::from_secs(4);

/// One frame: an H.264 access unit in Annex B form.
pub struct Frame {
    pub data: Vec<u8>,
    /// The recorder's timestamp in microseconds, from the clip's first frame.
    pub timestamp: u64,
}

#[derive(Default)]
pub struct Clip {
    pub frames: Vec<Frame>,
    /// Frames the console dropped because they weren't read fast enough.
    pub dropped: u32,
    /// Why it ended early, if it did.
    pub stopped: Option<proto::CaptureState>,
    /// No keyframe arrived within [`KEYFRAME_WAIT`].
    pub no_keyframe: bool,
    /// Wall time from the first frame to the last.
    pub elapsed: Duration,
    /// The sound from about the first frame on (48 kHz s16le stereo), if the console was capturing it.
    pub audio: Vec<u8>,
    /// The audio stream position to read from next (`u64::MAX`: no sound for this clip).
    pub audio_from: Option<u64>,
}

impl Clip {
    /// Frames per second, from the recorder's timestamps (or the wall time the frames arrived in); the
    /// console records 30.
    pub fn fps(&self) -> f64 {
        let last_us = self.frames.last().map_or(0, |f| f.timestamp);
        let secs = if last_us > 0 { last_us as f64 / 1e6 } else { self.elapsed.as_secs_f64() };
        if self.frames.len() < 2 || secs <= 0.0 { 30.0 } else { ((self.frames.len() - 1) as f64 / secs).clamp(1.0, 60.0) }
    }

    pub fn bytes(&self) -> usize {
        self.frames.iter().map(|f| f.data.len()).sum()
    }

    /// The raw H.264 elementary stream, with the parameter sets the recorder leaves out put before every
    /// keyframe (so the stream decodes from any of them).
    pub fn annex_b(&self) -> Vec<u8> {
        let sets = parameter_sets();
        let mut out = Vec::with_capacity(self.bytes() + self.frames.len() / 8 * sets.len() + sets.len());
        for f in &self.frames {
            if starts_idr(&f.data) {
                out.extend_from_slice(&sets);
            }
            out.extend_from_slice(&f.data);
        }
        out
    }
}

/// Whether an access unit's first NAL unit is an IDR slice (type 5).
fn starts_idr(data: &[u8]) -> bool {
    let at = if data.starts_with(&[0, 0, 0, 1]) { 4 } else if data.starts_with(&[0, 0, 1]) { 3 } else { return false };
    data.get(at).is_some_and(|b| b & 0x1F == 5)
}

/// Writes an H.264 RBSP bit by bit (MSB first), with Exp-Golomb codes.
struct Bits {
    out: Vec<u8>,
    used: u8,
}

impl Bits {
    fn new(nal_header: u8) -> Self {
        Bits { out: vec![0, 0, 0, 1, nal_header], used: 8 }
    }

    fn bit(&mut self, b: bool) {
        if self.used == 8 {
            self.out.push(0);
            self.used = 0;
        }
        if b {
            *self.out.last_mut().unwrap() |= 0x80 >> self.used;
        }
        self.used += 1;
    }

    fn bits(&mut self, value: u32, n: u8) {
        for i in (0..n).rev() {
            self.bit(value >> i & 1 == 1);
        }
    }

    /// ue(v).
    fn ue(&mut self, value: u32) {
        let v = value + 1;
        let len = 32 - v.leading_zeros() as u8;
        self.bits(0, len - 1);
        self.bits(v, len);
    }

    /// The stop bit and alignment. (No emulation prevention needed: these never hold 00 00 0x.)
    fn finish(mut self) -> Vec<u8> {
        self.bit(true);
        self.out
    }
}

/// The sequence and picture parameter sets of the console's gameplay recording, which `grc:d` doesn't send
/// (its stream starts at a bare IDR slice). Written from the encoder's settings, found by parsing what it
/// produces and checked by decoding real captures with ffmpeg: High profile, level 3.2, 4:2:0 8-bit,
/// 1280x720 progressive, one reference frame, frame_num of 8 bits, picture order count type 2, CABAC,
/// 8x8 transforms, deblocking control present, BT.709 colours with the sRGB transfer curve.
pub fn parameter_sets() -> Vec<u8> {
    let mut sps = Bits::new(0x67);
    sps.bits(100, 8); // profile_idc: High
    sps.bits(0b0000_1100, 8); // constraint_set4/5: progressive, no B frames
    sps.bits(32, 8); // level_idc 3.2
    sps.ue(0); // seq_parameter_set_id
    sps.ue(1); // chroma_format_idc: 4:2:0
    sps.ue(0); // bit_depth_luma_minus8
    sps.ue(0); // bit_depth_chroma_minus8
    sps.bit(false); // qpprime_y_zero_transform_bypass
    sps.bit(false); // seq_scaling_matrix_present
    sps.ue(8 - 4); // log2_max_frame_num_minus4
    sps.ue(2); // pic_order_cnt_type
    sps.ue(1); // max_num_ref_frames
    sps.bit(false); // gaps_in_frame_num_value_allowed
    sps.ue(1280 / 16 - 1); // pic_width_in_mbs_minus1
    sps.ue(720 / 16 - 1); // pic_height_in_map_units_minus1
    sps.bit(true); // frame_mbs_only
    sps.bit(true); // direct_8x8_inference
    sps.bit(false); // frame_cropping
    sps.bit(true); // vui_parameters_present
    sps.bit(false); // aspect_ratio_info_present
    sps.bit(false); // overscan_info_present
    sps.bit(true); // video_signal_type_present
    sps.bits(5, 3); // video_format: unspecified
    sps.bit(false); // video_full_range
    sps.bit(true); // colour_description_present
    sps.bits(1, 8); // colour_primaries: BT.709
    sps.bits(13, 8); // transfer_characteristics: sRGB
    sps.bits(1, 8); // matrix_coefficients: BT.709
    sps.bit(true); // chroma_loc_info_present
    sps.ue(0); // chroma_sample_loc_type_top_field
    sps.ue(0); // chroma_sample_loc_type_bottom_field
    sps.bit(false); // timing_info_present
    sps.bit(false); // nal_hrd_parameters_present
    sps.bit(false); // vcl_hrd_parameters_present
    sps.bit(false); // pic_struct_present
    sps.bit(false); // bitstream_restriction
    let mut pps = Bits::new(0x68);
    pps.ue(0); // pic_parameter_set_id
    pps.ue(0); // seq_parameter_set_id
    pps.bit(true); // entropy_coding_mode: CABAC
    pps.bit(false); // bottom_field_pic_order_in_frame_present
    pps.ue(0); // num_slice_groups_minus1
    pps.ue(0); // num_ref_idx_l0_default_active_minus1
    pps.ue(0); // num_ref_idx_l1_default_active_minus1
    pps.bit(false); // weighted_pred
    pps.bits(0, 2); // weighted_bipred_idc
    pps.ue(0); // pic_init_qp_minus26 (se 0)
    pps.ue(0); // pic_init_qs_minus26 (se 0)
    pps.ue(0); // chroma_qp_index_offset (se 0)
    pps.bit(true); // deblocking_filter_control_present
    pps.bit(false); // constrained_intra_pred
    pps.bit(false); // redundant_pic_cnt_present
    pps.bit(true); // transform_8x8_mode
    pps.bit(false); // pic_scaling_matrix_present
    pps.ue(0); // second_chroma_qp_index_offset (se 0)
    let mut out = sps.finish();
    out.extend(pps.finish());
    out
}

pub fn ffmpeg_available() -> bool {
    crate::capture::ffmpeg_available()
}

/// Writes `clip` to `dest`: an `.mp4` (the video stream copied, no re-encode; its sound as AAC) when ffmpeg is
/// installed, else the raw `.h264` stream next to it. Returns the file written.
pub fn save(clip: &Clip, dest: &Path) -> Result<PathBuf> {
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let raw = dest.with_extension("h264");
    std::fs::write(&raw, clip.annex_b()).with_context(|| format!("writing {}", raw.display()))?;
    if !ffmpeg_available() {
        return Ok(raw);
    }
    let mp4 = dest.with_extension("mp4");
    let pcm = dest.with_extension("pcm");
    let fps = format!("{:.3}", clip.fps());
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-loglevel", "error", "-f", "h264", "-i"]).arg(&raw);
    let with_sound = !clip.audio.is_empty() && std::fs::write(&pcm, &clip.audio).is_ok();
    if with_sound {
        cmd.args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i"]).arg(&pcm).args(["-map", "0:v", "-map", "1:a", "-c:a", "aac", "-shortest"]);
    }
    // A raw H.264 stream has no timestamps (and this one no timing info): number the frames at the clip's rate.
    let status = cmd
        .args(["-c:v", "copy", "-bsf:v"])
        .arg(format!("setts=pts=N/({fps}*TB):dts=N/({fps}*TB)"))
        .args(["-movflags", "+faststart"])
        .arg(&mp4)
        .status();
    let _ = std::fs::remove_file(&pcm);
    if status.is_ok_and(|s| s.success()) && mp4.exists() {
        let _ = std::fs::remove_file(&raw);
        Ok(mp4)
    } else {
        Ok(raw)
    }
}

/// Decodes `video` (an .mp4 or .h264 file) with ffmpeg into JPEG [`Shot`]s, `fps` a second, `width` wide.
pub fn decode_shots(video: &Path, clip_fps: f64, fps: u32, width: u32) -> Result<Vec<Shot>> {
    if !ffmpeg_available() {
        bail!("ffmpeg isn't installed, so the H.264 clip can't be turned into pictures");
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("kickstand-frames-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let result = (|| {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-loglevel", "error"]);
        if video.extension().is_some_and(|e| e == "h264") {
            cmd.args(["-r"]).arg(format!("{clip_fps:.3}")).args(["-f", "h264"]);
        }
        let status = cmd
            .arg("-i")
            .arg(video)
            .arg("-vf")
            .arg(format!("fps={fps},scale={width}:-2"))
            .args(["-q:v", "4"])
            .arg(dir.join("f_%04d.jpg"))
            .status()
            .context("running ffmpeg")?;
        if !status.success() {
            bail!("ffmpeg couldn't decode {}", video.display());
        }
        let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        names.sort();
        let step = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
        names
            .iter()
            .enumerate()
            .map(|(i, p)| Ok(Shot { at: step * i as u32, jpeg: std::fs::read(p)? }))
            .collect::<Result<Vec<_>>>()
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Streams about `seconds` of the console's gameplay video and saves it next to `near` (same name, `.mp4`).
/// Returns the file, the clip's frame rate, frame count and size in KB. Errors say why it couldn't, for a
/// caller to fall back to screenshots: no ffmpeg (needed to make anything of H.264 here), an older
/// console, nothing recording gameplay, or no keyframe in time.
pub async fn record_to_file(client: &crate::client::Client, seconds: f64, near: &Path) -> Result<(PathBuf, f64, usize, usize)> {
    if !ffmpeg_available() {
        bail!("ffmpeg isn't installed on this PC; install it to get the console's 30 fps video");
    }
    let clip = client.record_video(seconds).await?;
    if clip.frames.is_empty() {
        if let Some(state) = &clip.stopped
            && *state != proto::CaptureState::Capturing
        {
            let capture = client.audio_capture().await?;
            let why = crate::audio::why_silent(&capture).unwrap_or_else(|| format!("{state:?}"));
            bail!("{}", why.replace("no sound", "nothing"));
        }
        if clip.no_keyframe {
            bail!("the recorder sent no keyframe within {} s, so the video couldn't start", KEYFRAME_WAIT.as_secs());
        }
        bail!("no video frames arrived");
    }
    let (fps, frames, kb) = (clip.fps(), clip.frames.len(), clip.bytes() / 1024);
    let dest = near.with_extension("mp4");
    let saved = tokio::task::spawn_blocking(move || save(&clip, &dest)).await??;
    Ok((saved, fps, frames, kb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_comes_from_frames_over_time() {
        let mut clip = Clip::default();
        assert_eq!(clip.fps(), 30.0);
        // A keyframe, then predicted frames.
        clip.frames.push(Frame { data: vec![0, 0, 0, 1, 0x65, 0xB8], timestamp: 0 });
        for i in 1..31 {
            clip.frames.push(Frame { data: vec![0, 0, 0, 1, 0x41, i], timestamp: 0 });
        }
        clip.elapsed = Duration::from_secs(1);
        assert!((clip.fps() - 30.0).abs() < 1e-9);
        // The parameter sets go in front of the keyframe only.
        let stream = clip.annex_b();
        assert_eq!(stream.len(), parameter_sets().len() + 31 * 6);
        assert!(stream.starts_with(&parameter_sets()));
    }

    #[test]
    fn parameter_sets_match_the_consoles_encoder() {
        // What decoded the console's grc:d stream on hardware (Mario Party Superstars, fw 23.0.1): checked by
        // muxing a capture with these in front and decoding it with ffmpeg (High, 1280x720, level 3.2).
        let hex: String = parameter_sets().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "0000000167640c20ac2b402802dd35010d01e080".to_owned() + "0000000168ee3cb0");
    }
}
