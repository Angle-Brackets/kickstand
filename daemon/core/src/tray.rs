// SPDX-License-Identifier: GPL-3.0-only
//! Where the sysmodule draws its "an agent is working" indicator (the sysmodule's `indicator.rs` draws it,
//! shown while the keep-awake lease runs). Movable at runtime so it can be lined up with the home menu's
//! status bar.

use core::sync::atomic::{AtomicU32, Ordering};

/// Top-left corner in vi's layer space (1920x1080, even on the 720p panel): just left of the home menu's
/// clock, centred on it, with room for a wider time ("12:12 PM"). Lined up from photos of the console.
pub const DEFAULT_POS: (u16, u16) = (1374, 75);

static POS: AtomicU32 = AtomicU32::new(pack(DEFAULT_POS));

const fn pack((x, y): (u16, u16)) -> u32 {
    ((x as u32) << 16) | y as u32
}

pub fn position() -> (u16, u16) {
    let p = POS.load(Ordering::Relaxed);
    ((p >> 16) as u16, p as u16)
}

pub fn set_position(pos: (u16, u16)) {
    POS.store(pack(pos), Ordering::Relaxed);
}
