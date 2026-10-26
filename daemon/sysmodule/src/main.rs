// SPDX-License-Identifier: GPL-3.0-only
//! The daemon as a sysmodule: `atmosphere/contents/<program id>/exefs.nsp`. Logs to
//! `/switch-mcp/logs/daemon.log`. Develop without `boot2.flag` so a crash can't stop the console booting.

#![no_std]
#![no_main]

use daemon_core::server::{self, Config};
use daemon_core::{DaemonKind, log};
use nx::diag::abort;
use nx::diag::log::lm::LmLogger;
use nx::{svc, util};

nx::rrt0_define_module_name!("switch-mcp");
// A sysmodule must pick its own heap; the NRO default would take all free memory. 4 MiB covers the
// socket transfer memory (~0.6 MiB), two frame buffers and fs sessions.
nx::alloc_set_default_heap_size!(0x40_0000);
nx::rrt0_initialize_heap!();

#[unsafe(no_mangle)]
fn main() {
    // At boot the SD card and network can come up after us, so keep retrying.
    while daemon_core::sd_mount().is_err() {
        let _ = svc::sleep_thread(1_000_000_000);
    }
    daemon_core::log::enable_file();
    log!("switch-mcp daemon {} (sysmodule) starting", daemon_core::VERSION);

    let token = daemon_core::read_token_file();
    if token.is_none() {
        log!("no token in {}: every client will be refused", proto::path::TOKEN_PATH);
    }
    while let Err(rc) = server::init_sockets(DaemonKind::Sysmodule) {
        log!("socket init failed: {rc}; retrying");
        let _ = svc::sleep_thread(5_000_000_000);
    }
    server::run(&Config {
        kind: DaemonKind::Sysmodule,
        port: proto::DEFAULT_PORT,
        token: token.as_deref(),
        idle_timeout_ms: 120_000,
        stop: None,
    });
}

#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    // FatalThrow would put a fatal screen over whatever game is running; just end this process.
    util::simple_panic_handler::<LmLogger>(info, abort::AbortLevel::ProcessExit())
}
