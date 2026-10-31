// SPDX-License-Identifier: GPL-3.0-only
//! The "an agent is working" indicator: a 40x40 Kickstand icon on a vi:m managed layer above everything,
//! next to the home menu's clock, shown while the keep-awake lease runs and no software is open (a game,
//! hbloader, or the Album), so it never sits over a game. It's only on the Default and LCD
//! layer stacks, so screenshots and recordings don't include it. The GPU context and layer are made the
//! first time a lease starts, then kept. Hiding draws a transparent frame: SetLayerVisibility succeeds on
//! 23.0.1 but leaves a managed layer on screen.

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
/// The swing icon at 40x40, premultiplied RGBA as the compositor blends it (`assets/logo/gen_tray.py`).
const ICON: &[u8; (SIZE * SIZE * 4) as usize] = include_bytes!("tray-icon.rgba");
const POLL_NS: i64 = 500_000_000;
/// Polls between checks for open software (each check opens two pm sessions).
const SOFTWARE_EVERY: u32 = 4;
/// vi isn't ready for layers early in boot (2114-0006 at 10 s), so don't try before this uptime.
const EARLIEST_MS: u64 = 45_000;
/// After a failure to set up the layer, wait this long before trying again.
const RETRY_NS: i64 = 60_000_000_000;
/// The layer is only made once no software has been open for this long, and this long after the sysmodule
/// started. Made at the moment the Kickstand NRO had closed its own vi:m layer and handed over (the Album
/// still open), it never came back: vi stopped answering, then the home menu and every display
/// (2026-10-30; nothing logged after the start, the NRO's screen stayed blank and HOME did nothing).
const SETTLE_MS: u64 = 15_000;

type Canvas = CanvasManager<RGBA8>;

pub fn start() {
    let spawned = thread::Builder::new().name("indicator").stack_size(0x8000).spawn(run);
    if let Err(rc) = spawned {
        log!("indicator thread didn't start: {rc}");
    }
}

fn run() {
    let icon: Vec<RGBA8> = ICON.as_chunks::<4>().0.iter().map(|p| RGBA8::new_scaled(p[0], p[1], p[2], p[3])).collect();
    let blank = alloc::vec![RGBA8::new_scaled(0, 0, 0, 0); icon.len()];
    let mut layer: Option<(Canvas, (u16, u16))> = None;
    // What the layer shows now; a new layer starts blank.
    let mut shown = false;
    let mut software = false;
    let mut polls = 0u32;
    // Uptime when software was last seen open (or the thread started).
    let mut settled_from = daemon_core::uptime_ms();
    loop {
        if polls.is_multiple_of(SOFTWARE_EVERY) {
            software = software_open();
            if software {
                settled_from = daemon_core::uptime_ms();
            }
        }
        polls = polls.wrapping_add(1);
        let want = awake::remaining_ms() > 0 && !software;
        let pos = tray::position();
        // Moved (debug): make the layer again at the new spot.
        if layer.as_ref().is_some_and(|(_, at)| *at != pos) {
            layer = None;
            shown = false;
        }
        let now = daemon_core::uptime_ms();
        if want && layer.is_none() && now >= EARLIEST_MS && now >= settled_from + SETTLE_MS {
            match create(pos) {
                Ok(canvas) => layer = Some((canvas, pos)),
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
            match canvas.render_prepared_buffer(if want { &icon } else { &blank }) {
                Ok(()) => shown = want,
                Err(rc) => log!("indicator: drawing ({want}) failed: {rc}"),
            }
        }
        let _ = svc::sleep_thread(POLL_NS);
    }
}

/// Whether a game, hbloader or the Album is running. pm can't say whether it's in front, so a game
/// suspended behind the home menu counts too.
fn software_open() -> bool {
    let open = |found: Result<Option<services::pm::Application>>| found.is_ok_and(|app| app.is_some());
    open(services::pm::application()) || open(services::pm::album())
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
    log!("indicator: layer at ({x}, {y})");
    Ok(canvas)
}
