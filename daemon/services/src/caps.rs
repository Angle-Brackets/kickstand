// SPDX-License-Identifier: GPL-3.0-only
//! `caps:sc` (screen capture). Id and layout from libnx `source/services/capssc.c`.

use nx::ipc::sf::{self, sm};
use nx::result::Result;
use nx::service;

/// Official software's buffer for a 1280x720 JPEG.
pub const JPEG_BUFFER_SIZE: usize = 0x80000;
/// `ViLayerStack_Screenshot`: what the Capture button saves, without our indicator.
pub const LAYER_STACK_SCREENSHOT: u32 = 2;

/// The input of `capsscCaptureJpegScreenShot`.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct CaptureJpegArgs {
    pub layer_stack: u32,
    pub pad: u32,
    /// Nanoseconds.
    pub timeout: i64,
}

#[nx_derive::ipc_trait]
pub trait ScreenCaptureInterface {
    /// `capsscCaptureJpegScreenShot` (9.0+): returns the JPEG's size.
    #[ipc_rid(1204)]
    fn capture_jpeg_screenshot(&self, args: CaptureJpegArgs, jpeg: sf::OutNonSecureMapAliasBuffer<u8>) -> u64;
}

nx::ipc_client_define_client_default!(ScreenCaptureService);
impl IScreenCaptureInterfaceClient for ScreenCaptureService {}

impl service::IService for ScreenCaptureService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("caps:sc")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Captures the screen as a JPEG into `buf` (at least [`JPEG_BUFFER_SIZE`]); returns its length.
pub fn capture_jpeg(buf: &mut [u8]) -> Result<usize> {
    let srv = service::new_service_object::<ScreenCaptureService>()?;
    let args = CaptureJpegArgs { layer_stack: LAYER_STACK_SCREENSHOT, pad: 0, timeout: 1_000_000_000 };
    let size = srv.capture_jpeg_screenshot(args, sf::Buffer::from_mut_array(buf))?;
    Ok(size as usize)
}
