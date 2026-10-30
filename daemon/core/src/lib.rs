// SPDX-License-Identifier: GPL-3.0-only
//! The kickstand daemon. The same code runs as the boot-time sysmodule (`sysmodule/`) and as an NRO
//! launched from hbmenu (`app/`), which is how it's tested before anything is installed.

#![no_std]

extern crate alloc;

pub mod awake;
pub mod log;
mod sd;
pub mod server;
pub mod session;
pub mod tray;

pub use proto::DaemonKind;

/// Set when a client asked the NRO daemon to hand over to the sysmodule; the NRO starts it on exit.
pub static HANDOVER_REQUESTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Starts the installed sysmodule; returns its pid.
pub fn start_sysmodule() -> nx::result::Result<u64> {
    services::pm::launch_sd_program(proto::path::SYSMODULE_PROGRAM_ID)
}

/// The daemon's version, reported in [`proto::Response::Hello`].
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Milliseconds since the console booted.
pub fn uptime_ms() -> u64 {
    nx::arm::get_system_tick() * 1000 / nx::arm::get_system_tick_frequency()
}

/// Mounts the SD card as `sdmc`.
pub fn sd_mount() -> nx::result::Result<()> {
    sd::mount()
}

/// Reads the shared token from [`proto::path::TOKEN_PATH`], ignoring surrounding whitespace.
pub fn read_token_file() -> Option<alloc::string::String> {
    let (data, _) = sd::read(proto::path::TOKEN_PATH, 0, 256).ok()?;
    let token = core::str::from_utf8(&data).ok()?.trim();
    (!token.is_empty()).then(|| alloc::string::String::from(token))
}
