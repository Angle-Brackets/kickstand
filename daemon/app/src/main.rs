// SPDX-License-Identifier: GPL-3.0-only
//! The daemon as an NRO, opened from hbmenu (or nxlink, which can add a token:
//! `nxlink -s -a <switch-ip> --args "--token <token>" kickstand.nro`). It accepts the tokens in
//! `/kickstand/token.txt` like the sysmodule, and pairs new PCs with the code it shows. Press + on the
//! console to exit; a client's `Exit` request also ends it. The screen shows whether an
//! agent is connected and what it's doing, how to reach the console, and the daemon's log. Leaving with +
//! restarts the sysmodule it stopped (or one set to start at boot), so the console isn't left without
//! Kickstand.
//!
//! With `--open <nro> [args]` or `--title <id>` it is a launcher instead (see `launcher.rs`).

#![no_std]
#![no_main]

extern crate alloc;

mod launcher;
mod screen;

use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

use daemon_core::server::{self, Config};
use daemon_core::{DaemonKind, log};
use nx::diag::abort;
use nx::diag::log::lm::LmLogger;
use nx::service::hid;
use nx::{input, thread, util};

nx::rrt0_define_module_name!("kickstand");
nx::rrt0_initialize_heap!();

static STOP: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
fn main() {
    if let Some(request) = launcher::requested() {
        launch(request);
        return;
    }
    daemon_core::log::enable_recent();
    server::init_sockets(DaemonKind::Nro).unwrap();
    let _ = nxlink::connect();
    if let Err(rc) = daemon_core::sd_mount() {
        log!("mounting the SD card failed: {rc}");
        return;
    }
    // No keep-awake thread here: it never ends, and hbloader would keep running it after this NRO exits.

    if daemon_core::tokens::load(token_arg().as_deref()) == 0 {
        log!("not paired with any PC yet");
    }
    daemon_core::pairing::start();
    log!("kickstand daemon {} (NRO)", daemon_core::VERSION);
    // The sysmodule would hold the port; this NRO takes over until a client asks for a handover.
    let mut stopped_sysmodule = false;
    if services::pm::process_id(proto::path::SYSMODULE_PROGRAM_ID).is_some() {
        match stop_sysmodule() {
            Some(Ok(())) => {
                stopped_sysmodule = true;
                log!("paused the background service while this screen is open");
            }
            Some(Err(rc)) => log!("couldn't stop the running sysmodule ({rc}); the port may be taken"),
            None => log!("the background service didn't stop within {STOP_WAIT_MS} ms: it's stuck; restart the Switch"),
        }
    }

    let server = thread::Builder::new()
        .name("server")
        .stack_size(0x10000)
        .spawn(move || {
            // Client threads borrow the config; it lives until the process exits.
            server::run(alloc::boxed::Box::leak(alloc::boxed::Box::new(Config {
                kind: DaemonKind::Nro,
                port: proto::DEFAULT_PORT,
                idle_timeout_ms: 120_000,
                stop: Some(&STOP),
            })))
        })
        .unwrap();

    if run_screen() {
        log!("+ pressed");
        if stopped_sysmodule || BOOT2.load(Ordering::Relaxed) {
            daemon_core::HANDOVER_REQUESTED.store(true, Ordering::Relaxed);
        }
    }
    STOP.store(true, Ordering::Relaxed);
    let _ = server.join();
    services::hid::release_hold();
    // The listener is closed now, so the sysmodule can bind the port on its first try.
    if daemon_core::HANDOVER_REQUESTED.load(Ordering::Relaxed) {
        match daemon_core::start_sysmodule() {
            Ok(pid) => log!("handed over to the sysmodule (pid {pid})"),
            Err(rc) => log!("starting the sysmodule failed: {rc}"),
        }
    }
    log!("exiting");
    nxlink::disconnect();
}

/// How long [`stop_sysmodule`] waits.
const STOP_WAIT_MS: u64 = 5_000;

/// Terminates the sysmodule, giving up after [`STOP_WAIT_MS`] (`None`): a sysmodule stuck in a call to a
/// system service doesn't end, and waiting on it kept this screen from ever drawing (2026-10-30). The
/// thread is left behind then; the console needs a restart anyway.
fn stop_sysmodule() -> Option<nx::result::Result<()>> {
    use core::sync::atomic::AtomicU32;
    // 0: running, 1: stopped, otherwise the error's value.
    static OUTCOME: AtomicU32 = AtomicU32::new(0);
    let spawned = thread::Builder::new().name("stop-sysmodule").stack_size(0x4000).spawn(|| {
        let outcome = match services::pm::terminate_program(proto::path::SYSMODULE_PROGRAM_ID) {
            Ok(()) => 1,
            Err(rc) => rc.get_value().max(2),
        };
        OUTCOME.store(outcome, Ordering::Release);
    });
    if let Err(rc) = spawned {
        return Some(Err(rc));
    }
    let deadline = daemon_core::uptime_ms() + STOP_WAIT_MS;
    while daemon_core::uptime_ms() < deadline {
        match OUTCOME.load(Ordering::Acquire) {
            0 => {
                let _ = nx::svc::sleep_thread(100_000_000);
            }
            1 => return Some(Ok(())),
            rc => return Some(Err(nx::result::ResultCode::new(rc))),
        }
    }
    None
}

/// Launcher mode: no sockets, screen or sysmodule handling, just the launch (logged to the SD card).
fn launch(request: Result<launcher::Launch, String>) {
    if daemon_core::sd_mount().is_ok() {
        daemon_core::log::enable_file();
    }
    let result = request.map_err(|e| alloc::format!("bad request: {e}")).and_then(|launch| {
        launcher::run(launch).map_err(|rc| alloc::format!("{rc}"))
    });
    if let Err(e) = result {
        log!("launcher: {e}");
    }
}

fn token_arg() -> Option<String> {
    let mut args = nxlink::args();
    while let Some(arg) = args.next() {
        if arg == "--token" {
            return args.next().map(String::from);
        }
    }
    None
}

/// Whether the sysmodule is installed to start at boot, as the screen last saw.
static BOOT2: AtomicBool = AtomicBool::new(false);

/// Shows the status screen until + is pressed (true) or a client asks the daemon to exit.
fn run_screen() -> bool {
    let styles = hid::NpadStyleTag::FullKey()
        | hid::NpadStyleTag::Handheld()
        | hid::NpadStyleTag::JoyDual()
        | hid::NpadStyleTag::JoyLeft()
        | hid::NpadStyleTag::JoyRight();
    let ctx = input::Context::new(styles, 1).unwrap();
    let mut players = [ctx.get_player(hid::NpadIdType::No1), ctx.get_player(hid::NpadIdType::Handheld)];
    let mut screen = screen::Screen::new();
    while !STOP.load(Ordering::Relaxed) {
        let mut down = hid::NpadButton::from(0);
        for p in &mut players {
            down |= p.get_buttons_down();
        }
        if down.contains(hid::NpadButton::Plus()) {
            return true;
        }
        screen.frame(down.contains(hid::NpadButton::Up()), down.contains(hid::NpadButton::Down()));
        // The sysmodule has a thread for this; one here would outlive the NRO.
        services::hid::expire_hold(daemon_core::uptime_ms());
        BOOT2.store(screen.starts_at_boot(), Ordering::Relaxed);
    }
    false
}

#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    util::simple_panic_handler::<LmLogger>(info, abort::AbortLevel::FatalThrow())
}
