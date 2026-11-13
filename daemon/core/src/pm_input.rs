// SPDX-License-Identifier: GPL-3.0-only
//! Keeping Atmosphère's pm able to read buttons, so holding R while starting a title opens hbmenu.
//!
//! Facts from libstratosphere `hid_api.cpp` (GPL-2.0; no code): pm sets hid up once for applet resource user
//! id 0 (`hidInitialize`, `hidInitializeNpad`, players 1-8 and handheld, NpadStandard | SystemExt) and from
//! then on reads held keys through `hidGetNpadStatesSystemExt`; it never sets hid up again. Every process
//! that isn't an applet shares id 0, so anything that deactivates npad there, or changes its supported
//! players or styles, leaves pm reading no buttons until a reboot: Atmosphère's override keys stop working
//! (holding R starts the game), while Kickstand's own `!R` entry still matches ("R not held").
//!
//! The vendored nx used to deactivate npad for id 0 whenever an `input::Context` was dropped (PATCHES.md
//! item 7), which Kickstand's NRO screen and macro recording both did.

use nx::input;
use nx::service::hid;

/// The styles pm asks hid for: NpadStandard | SystemExt.
pub fn styles() -> hid::NpadStyleTag {
    hid::NpadStyleTag::FullKey()
        | hid::NpadStyleTag::Handheld()
        | hid::NpadStyleTag::JoyDual()
        | hid::NpadStyleTag::JoyLeft()
        | hid::NpadStyleTag::JoyRight()
        | hid::NpadStyleTag::SystemExt()
}

/// Players 1-8 (nx adds handheld, since [`styles`] includes it).
pub const PLAYERS: usize = 8;

/// Activates npad for id 0 with exactly pm's settings and keeps the session open for good. Run at start: it
/// repairs a console where something deactivated npad for id 0 earlier in this boot, without a reboot, and
/// never takes anything away (pm's own settings are the same).
pub fn restore() {
    match input::Context::new(styles(), PLAYERS) {
        // Dropping it would deactivate npad again on an nx without the fix; keep it for the process's life.
        Ok(ctx) => core::mem::forget(ctx),
        Err(rc) => crate::log!("couldn't set hid up for title overrides: {rc}"),
    }
}
