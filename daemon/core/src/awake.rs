// SPDX-License-Identifier: GPL-3.0-only
//! Keep-awake: while a lease is running, a thread tells `idle:sys` the user is active, so the screen
//! neither dims nor sleeps. Every client request extends the lease by [`ACTIVITY_LEASE_MS`], so the
//! console stays up while an agent works and goes back to its normal timers (sparing the battery) once it
//! stops; [`proto::Request::KeepAwake`] can hold it longer. The lease is saved to the SD card so that a
//! restart of the daemon (an update, or the NRO taking over) keeps it: losing it once let the console
//! fall asleep ten minutes after an update, under an agent that was still working (2026-10-31). A reboot
//! ends it.
//!
//! A sleeping console suspends the daemon too, so this can only prevent sleep, not end it. The lock screen
//! sleeps on its own timer, which `idle:sys` doesn't reset; [`unlock_after_agent_reboot`] gets past it.

use core::sync::atomic::{AtomicU64, Ordering};

use nx::svc;
use nx::thread;
use proto::path::AGENT_REBOOT_FLAG;
use services::idle::{IIdleSysInterfaceClient, IdleSysService};

use alloc::format;

use crate::{log, sd, uptime_ms};

/// Uptime (ms) until which the console is kept awake.
static UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// How long one client request keeps the console awake. Erring long: a console that sleeps between an
/// agent's tool calls (a long build) can't be woken remotely, while a lease that runs on costs a little
/// battery and is visible as the indicator.
pub const ACTIVITY_LEASE_MS: u64 = 30 * 60_000;
/// Longest lease [`proto::Request::KeepAwake`] may ask for.
pub const MAX_LEASE_MS: u64 = 8 * 3_600_000;

/// Where the lease outlives a restart of the daemon: "<until> <saved at>", both uptimes in ms.
const LEASE_PATH: &str = "/kickstand/keep-awake";
/// Every request extends the lease, so it's saved only when it has grown this much (or was ended): a
/// restart costs at most this much of it.
const SAVE_STEP_MS: u64 = 5 * 60_000;
/// The lease as last saved.
static SAVED_MS: AtomicU64 = AtomicU64::new(0);

/// Well inside the shortest dim and sleep timers (one minute).
const INTERVAL_NS: i64 = 20_000_000_000;

/// Keeps the console awake for at least `ms` from now.
pub fn extend(ms: u64) {
    UNTIL_MS.fetch_max(uptime_ms() + ms, Ordering::Relaxed);
}

/// Ends any lease; the console's own dim and sleep timers apply again.
pub fn clear() {
    UNTIL_MS.store(0, Ordering::Relaxed);
}

/// Milliseconds left on the lease, 0 if none.
pub fn remaining_ms() -> u64 {
    UNTIL_MS.load(Ordering::Relaxed).saturating_sub(uptime_ms())
}

/// Starts the thread that acts on the lease, taking up the lease a previous run of the daemon saved.
pub fn start() {
    restore();
    let spawned = thread::Builder::new().name("keep-awake").stack_size(0x4000).spawn(run);
    if let Err(rc) = spawned {
        log!("keep-awake thread didn't start: {rc}");
    }
}

/// Takes up the lease saved by the daemon that ran before this one since the console booted.
fn restore() {
    let Ok((data, _)) = sd::read(LEASE_PATH, 0, 64) else { return };
    let text = core::str::from_utf8(&data).unwrap_or("");
    let mut numbers = text.split_whitespace().map(|n| n.parse::<u64>().ok());
    let (Some(Some(until)), Some(Some(saved_at))) = (numbers.next(), numbers.next()) else { return };
    let now = uptime_ms();
    // Uptime starts over at a reboot, so a lease saved later than now is from before one.
    if saved_at > now || until <= now || until - now > MAX_LEASE_MS {
        return;
    }
    extend(until - now);
    SAVED_MS.store(until, Ordering::Relaxed);
    log!("keep-awake: {} min left, from before the restart", (until - now) / 60_000);
}

/// Saves the lease when it has grown by [`SAVE_STEP_MS`] or ended.
fn save_if_changed() {
    let until = UNTIL_MS.load(Ordering::Relaxed);
    let saved = SAVED_MS.load(Ordering::Relaxed);
    if until == saved || (until > saved && until - saved < SAVE_STEP_MS) {
        return;
    }
    SAVED_MS.store(until, Ordering::Relaxed);
    let text = format!("{until} {}", uptime_ms());
    let written = sd::create_dirs("/kickstand").and_then(|()| sd::remove_if_exists(LEASE_PATH)).and_then(|()| {
        let mut file = sd::create_for_write(LEASE_PATH, text.len() as u64)?;
        file.write_array::<u8, true>(text.as_bytes()).map_err(|rc| sd::io_error("write", LEASE_PATH, rc))
    });
    if let Err(e) = written {
        log!("keep-awake: saving the lease: {}", e.message);
    }
}

fn run() {
    let mut idle: Option<IdleSysService> = None;
    let mut failing = false;
    loop {
        save_if_changed();
        if remaining_ms() > 0 {
            let result = match idle.as_ref() {
                Some(srv) => srv.report_user_is_active(),
                None => nx::service::new_service_object::<IdleSysService>().and_then(|srv| {
                    let r = srv.report_user_is_active();
                    idle = Some(srv);
                    r
                }),
            };
            // Log a failure once, not every interval.
            match result {
                Err(rc) if !failing => {
                    log!("keep awake: idle:sys failed: {rc}");
                    failing = true;
                    idle = None;
                }
                Err(_) => idle = None,
                Ok(()) => failing = false,
            }
        } else {
            // Hold no session while idle.
            idle = None;
        }
        let _ = svc::sleep_thread(INTERVAL_NS);
    }
}

/// Uptimes (ms) at which to press B three times after an agent's reboot. Nothing says when the lock screen
/// is up, so press a few times across the first minutes; B does nothing on the home menu if it's already
/// unlocked. (A press at 53 s once came too early.)
const UNLOCK_AT_MS: [u64; 5] = [60_000, 90_000, 120_000, 150_000, 180_000];

/// Uptime (ms) of the last unlock press still to come, 0 if none.
static UNLOCKING_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// Milliseconds until the last unlock press after an agent reboot, if presses are still to come.
pub fn unlocking_ms() -> Option<u64> {
    UNLOCKING_UNTIL_MS.load(Ordering::Relaxed).checked_sub(uptime_ms()).filter(|&ms| ms > 0)
}

/// Sysmodule at boot: if the agent asked for this reboot ([`AGENT_REBOOT_FLAG`]), takes a lease and gets
/// past the lock screen. A boot the user started is left alone.
pub fn unlock_after_agent_reboot() {
    if sd::stat(AGENT_REBOOT_FLAG).is_err() {
        return;
    }
    if let Err(e) = sd::remove_if_exists(AGENT_REBOOT_FLAG) {
        // Don't keep unlocking on every later boot.
        log!("couldn't remove {AGENT_REBOOT_FLAG}: {}; not unlocking", e.message);
        return;
    }
    // Started well after boot (the NRO handing over after a boot where the sysmodule didn't run): the flag
    // is stale, and pressing now would land on whatever the user is doing.
    if uptime_ms() > UNLOCK_AT_MS[0] {
        log!("stale {AGENT_REBOOT_FLAG} (started at {}s): not unlocking", uptime_ms() / 1000);
        return;
    }
    let last = UNLOCK_AT_MS[UNLOCK_AT_MS.len() - 1];
    extend(last + ACTIVITY_LEASE_MS);
    UNLOCKING_UNTIL_MS.store(last, Ordering::Relaxed);
    let spawned = thread::Builder::new().name("unlock").stack_size(0x4000).spawn(|| {
        log!("booted by an agent reboot: unlocking");
        for at in UNLOCK_AT_MS {
            let now = uptime_ms();
            if at > now {
                let _ = svc::sleep_thread(((at - now) * 1_000_000) as i64);
            }
            if let Err(rc) = services::hid::press(proto::Button::B.bit(), 3, 100) {
                log!("unlock press failed: {rc}");
            }
        }
    });
    if let Err(rc) = spawned {
        UNLOCKING_UNTIL_MS.store(0, Ordering::Relaxed);
        log!("unlock thread didn't start: {rc}");
    }
}

/// Marks the coming reboot as the agent's, for [`unlock_after_agent_reboot`].
pub fn mark_agent_reboot() -> Result<(), proto::Error> {
    sd::create_for_write(AGENT_REBOOT_FLAG, 0).map(drop)
}

/// Undoes [`mark_agent_reboot`] when the reboot didn't happen.
pub fn unmark_agent_reboot() {
    let _ = sd::remove_if_exists(AGENT_REBOOT_FLAG);
}
