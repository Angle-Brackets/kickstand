// SPDX-License-Identifier: GPL-3.0-only
//! The daemon as an NRO. Start it with nxlink, passing the token as an argument:
//! `nxlink -s -a <switch-ip> --args "--token <token>" kickstand.nro`. Press + on the console to exit.
//! Without `--token` it uses `/kickstand/token.txt` like the sysmodule does. A client's `Exit` request
//! also ends it.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

use daemon_core::server::{self, Config};
use daemon_core::{DaemonKind, log};
use nx::diag::abort;
use nx::diag::log::lm::LmLogger;
use nx::service::hid;
use nx::{input, svc, thread, util};

nx::rrt0_define_module_name!("kickstand");
nx::rrt0_initialize_heap!();

static STOP: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
fn main() {
    server::init_sockets(DaemonKind::Nro).unwrap();
    let _ = nxlink::connect();
    if let Err(rc) = daemon_core::sd_mount() {
        log!("mounting the SD card failed: {rc}");
        return;
    }

    let token = token_arg().or_else(daemon_core::read_token_file);
    if token.is_none() {
        log!("no token: pass --token <t> or create {}", proto::path::TOKEN_PATH);
    }
    log!("kickstand daemon {} (NRO); press + to exit", daemon_core::VERSION);

    let server = thread::Builder::new()
        .name("server")
        .stack_size(0x10000)
        .spawn(move || {
            server::run(&Config {
                kind: DaemonKind::Nro,
                port: proto::DEFAULT_PORT,
                token: token.as_deref(),
                idle_timeout_ms: 120_000,
                stop: Some(&STOP),
            })
        })
        .unwrap();

    wait_for_plus();
    STOP.store(true, Ordering::Relaxed);
    let _ = server.join();
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

fn token_arg() -> Option<String> {
    let mut args = nxlink::args();
    while let Some(arg) = args.next() {
        if arg == "--token" {
            return args.next().map(String::from);
        }
    }
    None
}

fn wait_for_plus() {
    let styles = hid::NpadStyleTag::FullKey()
        | hid::NpadStyleTag::Handheld()
        | hid::NpadStyleTag::JoyDual()
        | hid::NpadStyleTag::JoyLeft()
        | hid::NpadStyleTag::JoyRight();
    let ctx = input::Context::new(styles, 1).unwrap();
    let mut players = [ctx.get_player(hid::NpadIdType::No1), ctx.get_player(hid::NpadIdType::Handheld)];
    loop {
        if STOP.load(Ordering::Relaxed) {
            return;
        }
        if players.iter_mut().any(|p| p.get_buttons_down().contains(hid::NpadButton::Plus())) {
            return;
        }
        let _ = svc::sleep_thread(50_000_000);
    }
}

#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    util::simple_panic_handler::<LmLogger>(info, abort::AbortLevel::FatalThrow())
}
