// SPDX-License-Identifier: GPL-3.0-only
//! `grc:d` (`nn::grcsrv::IRemoteVideoTransfer`, 6.0.0+): the foreground application's gameplay recording as
//! it is made, audio as 48 kHz 16-bit stereo PCM and video as H.264 NAL units. Command ids and the Transfer
//! layout follow libnx `source/services/grc.c` and `include/switch/services/grc.h` (ISC); the behaviour
//! notes follow switchbrew's "GRC services" page.
//!
//! - Initialize (cmd 1) starts the stream. It may be called once per boot by anyone: a second call, even
//!   from another session or process, makes the grc sysmodule assert. So never call it blindly: call
//!   Transfer first, which answers [`RESULT_NOT_INITIALIZED`] while nobody has, and only then Initialize.
//! - Transfer (cmd 2) blocks until the recorder has data, and never returns while the running application
//!   doesn't record gameplay (none running, its NACP turns video capture off, or a Manual-mode title that
//!   didn't call `InitializeGamePlayRecording`). Read each stream from its own thread and session.
//! - grc:d allows 4 sessions in all and serves them on 2 threads.
//! - A process stopped while one of its Transfers is under way (blocked, e.g. with the game behind an
//!   applet, or left over after the game exited) leaves grc sending nothing to anyone until a reboot (seen
//!   on 23.0.1). Stopping a reader whose Transfers have all returned is fine, and so is the death of the
//!   process that called Initialize.

use nx::ipc::sf::{self, sm};
use nx::result::{Result, ResultCode};
use nx::service;

/// `GrcStream_Video`: H.264 NAL units, one frame per Transfer.
pub const STREAM_VIDEO: u32 = 0;
/// `GrcStream_Audio`: PCM s16le, 2 channels, 48000 Hz.
pub const STREAM_AUDIO: u32 = 1;
/// The buffer official software reads audio with.
pub const AUDIO_BUFFER_SIZE: usize = 0x1000;
/// The buffer official software reads video with.
pub const VIDEO_BUFFER_SIZE: usize = 0x32000;
/// What Transfer answers before anyone has called Initialize (2212-0500); also an invalid stream.
pub const RESULT_NOT_INITIALIZED: u32 = 0x3E8D4;

/// Transfer's output (libnx `grcdTransfer`).
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default, Debug)]
#[repr(C)]
pub struct TransferOut {
    pub num_frames: u32,
    pub data_size: u32,
    pub start_timestamp: u64,
}

#[nx_derive::ipc_trait]
pub trait RemoteVideoTransfer {
    /// `grcdBegin`: see the module docs before calling it.
    #[ipc_rid(1)]
    fn initialize(&self);
    /// `grcdTransfer`: blocks until `stream` has data.
    #[ipc_rid(2)]
    fn transfer(&self, stream: u32, buffer: sf::OutMapAliasBuffer<u8>) -> TransferOut;
}

nx::ipc_client_define_client_default!(RemoteVideoTransferService);
impl IRemoteVideoTransferClient for RemoteVideoTransferService {}

impl service::IService for RemoteVideoTransferService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("grc:d")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// One session to `grc:d`. Keep it for the life of the reader: sessions are few (4 for everyone).
pub struct Recorder(RemoteVideoTransferService);

impl Recorder {
    pub fn open() -> Result<Self> {
        Ok(Recorder(service::new_service_object::<RemoteVideoTransferService>()?))
    }

    /// Reads the next piece of `stream` into `buf` (blocking; see the module docs).
    pub fn transfer(&self, stream: u32, buf: &mut [u8]) -> Result<TransferOut> {
        self.0.transfer(stream, sf::Buffer::from_mut_array(buf))
    }

    /// Starts the stream for this boot. Only after Transfer answered [`RESULT_NOT_INITIALIZED`].
    pub fn initialize(&self) -> Result<()> {
        self.0.initialize()
    }
}

/// Whether `rc` is [`RESULT_NOT_INITIALIZED`].
pub fn not_initialized(rc: ResultCode) -> bool {
    rc.get_value() == RESULT_NOT_INITIALIZED
}
