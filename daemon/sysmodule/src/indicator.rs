// SPDX-License-Identifier: GPL-3.0-only
//! The "an agent is working" indicator: a 40x40 Kickstand icon on a vi:m managed layer above everything,
//! next to the home menu's clock, shown while the keep-awake lease runs. It's only on the Default and LCD
//! layer stacks, so screenshots and recordings don't include it. The GPU context and layer are made the
//! first time a lease starts, then kept and shown or hidden.

use alloc::sync::Arc;
use alloc::vec::Vec;

use daemon_core::{awake, log, tray};
use nx::gpu::canvas::{CanvasManager, RGBA8};
use nx::gpu::surface::{DisplayName, ScaleMode};
use nx::gpu::{self, BlockLinearHeights, LayerZ};
use nx::ipc::sf::AppletResourceUserId;
use nx::result::Result;
use nx::service::vi::{LayerFlags, LayerStackId};
use nx::svc;
use nx::sync::RwLock;
use nx::thread;

/// In vi's layer space, which is 1920x1080 whatever the output (so about 27 px on the 720p panel).
const SIZE: u32 = 40;
/// The swing icon at 40x40, straight RGBA (`assets/logo/gen_tray.py`).
const ICON: &[u8; (SIZE * SIZE * 4) as usize] = include_bytes!("tray-icon.rgba");
const POLL_NS: i64 = 500_000_000;
/// vi isn't ready for layers early in boot (2114-0006 at 10 s), so don't try before this uptime.
const EARLIEST_MS: u64 = 45_000;
/// After a failure to set up the layer, wait this long before trying again.
const RETRY_NS: i64 = 60_000_000_000;

type Canvas = CanvasManager<RGBA8>;

pub fn start() {
    let spawned = thread::Builder::new().name("indicator").stack_size(0x8000).spawn(run);
    if let Err(rc) = spawned {
        log!("indicator thread didn't start: {rc}");
    }
}

fn run() {
    let mut layer: Option<(Canvas, (u16, u16))> = None;
    let mut shown = false;
    loop {
        let want = awake::remaining_ms() > 0;
        let pos = tray::position();
        // Moved (debug): make the layer again at the new spot.
        if layer.as_ref().is_some_and(|(_, at)| *at != pos) {
            layer = None;
            shown = false;
        }
        if want && layer.is_none() && daemon_core::uptime_ms() >= EARLIEST_MS {
            match create(pos) {
                Ok(canvas) => {
                    layer = Some((canvas, pos));
                    shown = true;
                }
                Err(rc) => {
                    log!("indicator: couldn't set up the layer: {rc}");
                    let _ = svc::sleep_thread(RETRY_NS);
                    continue;
                }
            }
        }
        if let Some((canvas, _)) = layer.as_mut()
            && want != shown
        {
            match canvas.surface.set_visible(want) {
                Ok(()) => shown = want,
                Err(rc) => log!("indicator: set_visible({want}) failed: {rc}"),
            }
        }
        let _ = svc::sleep_thread(POLL_NS);
    }
}

// The context stays on this thread; nx's canvas takes it as `Arc<RwLock<_>>`.
#[allow(clippy::arc_with_non_send_sync)]
fn create((x, y): (u16, u16)) -> Result<Canvas> {
    let ctx = gpu::Context::new(gpu::NvDrvServiceKind::System, gpu::ViServiceKind::Manager, 0x40000)?;
    let mut canvas = Canvas::new_managed(
        Arc::new(RwLock::new(ctx)),
        DisplayName::Default,
        u32::from(x),
        u32::from(y),
        LayerZ::Max,
        SIZE,
        SIZE,
        AppletResourceUserId::new(0),
        LayerFlags::None(),
        2,
        BlockLinearHeights::OneGob,
        // As Tesla overlays do; ScaleMode::None fails on 23.0.1 (2114-0006).
        ScaleMode::FitToLayer { width: SIZE, height: SIZE },
    )?;
    canvas.surface.push_layer_stack(LayerStackId::Default)?;
    // Handheld mode's stack; harmless if this firmware doesn't need it.
    let _ = canvas.surface.push_layer_stack(LayerStackId::Lcd);
    let pixels: Vec<RGBA8> = ICON.chunks_exact(4).map(|p| RGBA8::new_scaled(p[0], p[1], p[2], p[3])).collect();
    canvas.render_prepared_buffer(&pixels)?;
    log!("indicator: layer at ({x}, {y})");
    Ok(canvas)
}
