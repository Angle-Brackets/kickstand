// SPDX-License-Identifier: GPL-3.0-only
//! The NRO's screen: a scrolling view of the daemon log, so the console shows what it's doing instead of
//! a black screen.

use alloc::sync::Arc;
use core::num::NonZeroU16;

use nx::console::scrollback::ScrollbackConsole;
use nx::gpu;
use nx::svc;
use nx::sync::RwLock;

use daemon_core::log;

pub struct Screen {
    /// `None` if the display couldn't be set up; the daemon still runs, just without a picture.
    console: Option<ScrollbackConsole>,
}

impl Screen {
    // nx's console takes the GPU context as `Arc<RwLock<_>>`; it stays on this thread.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new() -> Self {
        let console = gpu::Context::new(gpu::NvDrvServiceKind::Applet, gpu::ViServiceKind::System, 0x40000)
            .and_then(|ctx| {
                ScrollbackConsole::new(Arc::new(RwLock::new(ctx)), 300, NonZeroU16::new(90).unwrap(), true, None, 2)
            });
        match console {
            Ok(console) => Screen { console: Some(console) },
            Err(rc) => {
                log!("no screen output: {rc}");
                Screen { console: None }
            }
        }
    }

    /// Shows new log lines and waits for the next frame (or 50 ms without a display).
    pub fn frame(&mut self, scroll_up: bool, scroll_down: bool) {
        let Some(console) = self.console.as_mut() else {
            let _ = log::take_recent();
            let _ = svc::sleep_thread(50_000_000);
            return;
        };
        if scroll_up {
            console.scroll_up();
        } else if scroll_down {
            console.scroll_down();
        }
        for line in log::take_recent() {
            console.write(line);
        }
        if console.draw().is_err() || console.wait_vsync_event(None).is_err() {
            let _ = svc::sleep_thread(16_000_000);
        }
    }
}
