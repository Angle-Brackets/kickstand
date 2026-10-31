// SPDX-License-Identifier: GPL-3.0-only
//! The kickstand daemon. The same code runs as the boot-time sysmodule (`sysmodule/`) and as an NRO
//! launched from hbmenu (`app/`), which is how it's tested before anything is installed.

#![no_std]

extern crate alloc;

pub mod activity;
pub mod awake;
pub mod fatal;
pub mod hbl;
pub mod log;
pub mod pairing;
pub mod sd;
pub mod server;
pub mod session;
pub mod tokens;
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
/// Releases a held virtual controller ([`proto::Request::Hold`]) when its lease runs out, checking ten
/// times a second. For the sysmodule; the NRO checks from its screen loop instead, as a thread left
/// running after it exits would run unmapped code.
pub fn start_hold_expiry() {
    let spawned = nx::thread::Builder::new().name("hold").stack_size(0x4000).spawn(|| {
        loop {
            services::hid::expire_hold(uptime_ms());
            let _ = nx::svc::sleep_thread(100_000_000);
        }
    });
    if let Err(rc) = spawned {
        log!("hold expiry thread didn't start: {rc}");
    }
}

pub fn uptime_ms() -> u64 {
    nx::arm::get_system_tick() * 1000 / nx::arm::get_system_tick_frequency()
}

/// Mounts the SD card as `sdmc`.
pub fn sd_mount() -> nx::result::Result<()> {
    sd::mount()
}

