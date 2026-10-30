// SPDX-License-Identifier: GPL-3.0-only
//! Keep-awake: while on, a thread tells `idle:sys` the user is active, so the screen neither dims nor
//! sleeps. A sleeping console suspends the daemon too, so this can only prevent sleep, not end it. The
//! setting lives in [`KEEP_AWAKE_FLAG`] so it survives a reboot (and covers the lock screen after one).

use core::sync::atomic::{AtomicBool, Ordering};

use nx::svc;
use nx::thread;
use proto::path::KEEP_AWAKE_FLAG;
use services::idle::{IIdleSysInterfaceClient, IdleSysService};

use crate::{log, sd};

static ON: AtomicBool = AtomicBool::new(false);

/// Well inside the shortest dim and sleep timers (one minute).
const INTERVAL_NS: i64 = 20_000_000_000;

pub fn is_on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Turns keep-awake on or off and saves the choice for the next boot.
pub fn set(on: bool) -> Result<(), proto::Error> {
    if on {
        sd::create_for_write(KEEP_AWAKE_FLAG, 0).map(drop)?;
    } else {
        sd::remove_if_exists(KEEP_AWAKE_FLAG)?;
    }
    ON.store(on, Ordering::Relaxed);
    log!("keep awake {}", if on { "on" } else { "off" });
    Ok(())
}

/// Reads the saved setting and starts the thread that acts on it.
pub fn start() {
    ON.store(sd::stat(KEEP_AWAKE_FLAG).is_ok(), Ordering::Relaxed);
    let spawned = thread::Builder::new().name("keep-awake").stack_size(0x4000).spawn(run);
    if let Err(rc) = spawned {
        log!("keep-awake thread didn't start: {rc}");
    }
}

fn run() {
    let mut idle: Option<IdleSysService> = None;
    let mut failing = false;
    loop {
        if is_on() {
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
            // Hold no session while off.
            idle = None;
        }
        let _ = svc::sleep_thread(INTERVAL_NS);
    }
}
