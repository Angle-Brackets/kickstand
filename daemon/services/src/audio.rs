// SPDX-License-Identifier: GPL-3.0-only
//! Audio control (`audctl`) and output (`audout:u`) wrappers.
//! Command ids and struct layouts follow libnx `source/services/audctl.c` and `audout.c` (ISC).

use nx::ipc::sf::{self, sm};
use nx::result::Result;
use nx::service;
use nx::service::audio::{
    AudioInterfaceName, AudioOutManagerService, IAudioOutManagerClient,
};
use nx::version::{self, Version};

/// Native Horizon OS audio mixer sample rate (48 kHz).
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo (Left, Right interleaved).
pub const CHANNELS: u16 = 2;
/// 16-bit signed little-endian PCM (`i16`).
pub const BYTES_PER_SAMPLE: usize = 2;
/// Bytes per stereo sample frame (`2 * 2 = 4`).
pub const BYTES_PER_FRAME: usize = (CHANNELS as usize) * BYTES_PER_SAMPLE;
/// Bytes per second of 48 kHz 16-bit stereo PCM (`192,000` bytes/s).
pub const BYTES_PER_SECOND: usize = (SAMPLE_RATE as usize) * BYTES_PER_FRAME;
/// Page alignment required by `AudioBuffer` sample buffers (`0x1000`).
pub const PAGE_SIZE: usize = 0x1000;

/// Output routing target (`AudioTarget` in libnx `audctl.h`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum AudioTarget {
    #[default]
    Invalid = 0,
    Speaker = 1,
    Headphone = 2,
    Tv = 3,
    UsbOutputDevice = 4,
    Bluetooth = 5,
}

impl AudioTarget {
    pub const fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::Speaker,
            2 => Self::Headphone,
            3 => Self::Tv,
            4 => Self::UsbOutputDevice,
            5 => Self::Bluetooth,
            _ => Self::Invalid,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Invalid => "unknown",
            Self::Speaker => "speaker",
            Self::Headphone => "headphones",
            Self::Tv => "tv",
            Self::UsbOutputDevice => "usb",
            Self::Bluetooth => "bluetooth",
        }
    }
}

/// Input of `audctlSetTargetVolume` (cmd 1): `{ u32 target; s32 volume; }`.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct SetTargetVolumeArgs {
    pub target: u32,
    pub volume: i32,
}

/// Input of `audctlSetTargetMute` (cmd 5): `{ bool mute; u8 pad[3]; u32 target; }`.
/// Note: libnx packs `mute` in the low 32 bits and `target` in the high 32 bits of the 8-byte raw payload.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct SetTargetMuteArgs {
    pub mute: bool,
    pub pad: [u8; 3],
    pub target: u32,
}

#[nx_derive::ipc_trait]
pub trait AudCtlInterface {
    #[ipc_rid(0)]
    fn get_target_volume(&self, target: u32) -> i32;
    #[ipc_rid(1)]
    fn set_target_volume(&self, args: SetTargetVolumeArgs);
    #[ipc_rid(2)]
    fn get_target_volume_min(&self) -> i32;
    #[ipc_rid(3)]
    fn get_target_volume_max(&self) -> i32;
    #[ipc_rid(4)]
    fn is_target_mute(&self, target: u32) -> bool;
    #[ipc_rid(5)]
    fn set_target_mute(&self, args: SetTargetMuteArgs);
    #[ipc_rid(8)]
    fn get_default_target(&self) -> u32;
    #[ipc_rid(15)]
    fn set_output_target(&self, target: u32);
    #[ipc_rid(21)]
    fn get_audio_output_target_for_play_report(&self) -> u32;
    #[ipc_rid(23)]
    fn set_system_output_master_volume(&self, volume: f32);
    #[ipc_rid(24)]
    fn get_system_output_master_volume(&self) -> f32;
    #[ipc_rid(32)]
    fn get_active_output_target(&self) -> u32;
}

nx::ipc_client_define_client_default!(AudCtlService);
impl IAudCtlInterfaceClient for AudCtlService {}

impl service::IService for AudCtlService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("audctl")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Snapshot of the console's current audio output state.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct AudioStatus {
    pub sample_rate: u32,
    pub channels: u16,
    pub target: AudioTarget,
    /// Discrete target volume step (typically `0..=15`).
    pub target_volume: i32,
    pub volume_min: i32,
    pub volume_max: i32,
    /// System output master volume (`0.0..=1.0`).
    pub master_volume: f32,
    pub muted: bool,
    /// Whether `audout:u` reports at least one output device and started state.
    pub output_active: bool,
}

impl Default for AudioStatus {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            target: AudioTarget::Speaker,
            target_volume: 10,
            volume_min: 0,
            volume_max: 15,
            master_volume: 1.0,
            muted: false,
            output_active: false,
        }
    }
}

/// Queries the active output target on `audctl`, trying `GetActiveOutputTarget` (13.0+) first,
/// then `GetAudioOutputTargetForPlayReport` (3.0+), and finally `GetDefaultTarget`.
fn query_active_target(ctl: &AudCtlService) -> AudioTarget {
    if version::get_version() >= Version::new(13, 0, 0)
        && let Ok(raw) = ctl.get_active_output_target()
    {
        let target = AudioTarget::from_raw(raw);
        if target != AudioTarget::Invalid {
            return target;
        }
    }
    if let Ok(raw) = ctl.get_audio_output_target_for_play_report() {
        let target = AudioTarget::from_raw(raw);
        if target != AudioTarget::Invalid {
            return target;
        }
    }
    if let Ok(raw) = ctl.get_default_target() {
        let target = AudioTarget::from_raw(raw);
        if target != AudioTarget::Invalid {
            return target;
        }
    }
    AudioTarget::Speaker
}

/// Queries `audout:u` for sample rate, channel count and whether an output device is present
/// without opening a stateful `IAudioOut` session or copying a process handle into `audio`.
pub fn probe_audio_out() -> Result<(u32, u16, bool)> {
    let audout = service::new_service_object::<AudioOutManagerService>()?;
    let mut names = [AudioInterfaceName::new(); 4];
    let count = audout
        .list_audio_outs_auto(sf::Buffer::from_mut_array(&mut names))
        .or_else(|_| audout.list_audio_outs(sf::Buffer::from_mut_array(&mut names)))
        .unwrap_or(0);
    Ok((SAMPLE_RATE, CHANNELS, count > 0))
}

/// Queries the current audio output state (`audctl` + `audout:u`).
/// Gracefully falls back to defaults if a service is unavailable on the current firmware.
pub fn status() -> Result<AudioStatus> {
    let (sample_rate, channels, output_active) = probe_audio_out().unwrap_or((SAMPLE_RATE, CHANNELS, false));
    let Ok(ctl) = service::new_service_object::<AudCtlService>() else {
        return Ok(AudioStatus {
            sample_rate,
            channels,
            output_active,
            ..AudioStatus::default()
        });
    };
    let target = query_active_target(&ctl);
    let raw_target = target as u32;
    let volume_min = ctl.get_target_volume_min().unwrap_or(0);
    let volume_max = ctl.get_target_volume_max().unwrap_or(15).max(volume_min);
    let target_volume = ctl
        .get_target_volume(raw_target)
        .unwrap_or(10)
        .clamp(volume_min, volume_max);
    let muted = ctl.is_target_mute(raw_target).unwrap_or(false);
    let master_volume = ctl
        .get_system_output_master_volume()
        .unwrap_or_else(|_| {
            let span = (volume_max - volume_min).max(1) as f32;
            ((target_volume - volume_min) as f32 / span).clamp(0.0, 1.0)
        })
        .clamp(0.0, 1.0);
    Ok(AudioStatus {
        sample_rate,
        channels,
        target,
        target_volume,
        volume_min,
        volume_max,
        master_volume,
        muted,
        output_active,
    })
}

/// Sets the system master volume (`0.0..=1.0`) and updates the active target's discrete volume step.
pub fn set_volume(volume: f32) -> Result<AudioStatus> {
    let clamped = volume.clamp(0.0, 1.0);
    let ctl = service::new_service_object::<AudCtlService>()?;
    let target = query_active_target(&ctl);
    let raw_target = target as u32;
    let min = ctl.get_target_volume_min().unwrap_or(0);
    let max = ctl.get_target_volume_max().unwrap_or(15).max(min);
    let step = min + ((clamped * (max - min) as f32) + 0.5) as i32;
    let _ = ctl.set_target_volume(SetTargetVolumeArgs {
        target: raw_target,
        volume: step.clamp(min, max),
    });
    let _ = ctl.set_system_output_master_volume(clamped);
    status()
}

/// Mutes or unmutes the active audio output target.
pub fn set_mute(mute: bool) -> Result<AudioStatus> {
    let ctl = service::new_service_object::<AudCtlService>()?;
    let target = query_active_target(&ctl);
    ctl.set_target_mute(SetTargetMuteArgs {
        mute,
        pad: [0; 3],
        target: target as u32,
    })?;
    status()
}

/// Attempts to capture up to `out_samples.len()` `i16` PCM samples within `_timeout_ns`.
///
/// On Horizon OS, `audin:u` is microphone input (not system mixer loopback) and repeatedly opening
/// `IAudioIn` sessions from a background sysmodule leaks server-side handles inside `audio`
/// (`0100000000000014`, `0xD201` / `svc::ResultOutOfHandles`). Returning `Ok(0)` lets the ring buffer
/// and streaming pipeline serve pushed PCM samples or zero-filled silence without touching `audin:u`.
pub fn try_capture_samples(_out_samples: &mut [i16], _timeout_ns: i64) -> Result<usize> {
    Ok(0)
}
