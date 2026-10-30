// SPDX-License-Identifier: GPL-3.0-only
//! The daemon as a sysmodule: `atmosphere/contents/<program id>/exefs.nsp`. Logs to
//! `/kickstand/logs/daemon.log`. Develop without `boot2.flag` so a crash can't stop the console booting.

#![no_std]
#![no_main]

extern crate alloc;

mod indicator;

use daemon_core::server::{self, Config};
use daemon_core::{DaemonKind, log};
use nx::diag::abort;
use nx::diag::log::lm::LmLogger;
use nx::{svc, util};

nx::rrt0_define_module_name!("sys-kickstand");
// A sysmodule must pick its own heap; the NRO default would take all free memory. 8 MiB covers the
// socket transfer memory (~1.7 MiB), up to three clients (64 KiB stacks, frame buffers, a 512 KiB capture
// buffer each while they hold a screenshot), fs sessions and the indicator's GPU memory (~0.3 MiB). It comes
// out of the pool all system processes share (15.5 MB free with 4 MiB here, 13.4 MB with 6).
const HEAP_SIZE: usize = 0x80_0000;
// svcSetHeapSize takes multiples of 2 MiB only; 7 MiB made the sysmodule exit at startup.
const _: () = assert!(HEAP_SIZE.is_multiple_of(0x20_0000));
nx::alloc_set_default_heap_size!(HEAP_SIZE);
nx::rrt0_initialize_heap!();

#[unsafe(no_mangle)]
fn main() {
    // At boot the SD card and network can come up after us, so keep retrying.
    while daemon_core::sd_mount().is_err() {
        let _ = svc::sleep_thread(1_000_000_000);
    }
    daemon_core::log::enable_file();
    log!("kickstand daemon {} (sysmodule) starting", daemon_core::VERSION);

    daemon_core::fatal::note_start();
    daemon_core::awake::start();
    daemon_core::awake::unlock_after_agent_reboot();
    daemon_core::hbl::disarm_leftover();
    indicator::start();
    if daemon_core::tokens::load(None) == 0 {
        log!("no tokens in {}: pair a PC through the Kickstand NRO", proto::path::TOKEN_PATH);
    }
    while let Err(rc) = server::init_sockets(DaemonKind::Sysmodule) {
        log!("socket init failed: {rc}; retrying");
        let _ = svc::sleep_thread(5_000_000_000);
    }
    // Client threads borrow the config for as long as the process lives.
    server::run(alloc::boxed::Box::leak(alloc::boxed::Box::new(Config {
        kind: DaemonKind::Sysmodule,
        port: proto::DEFAULT_PORT,
        idle_timeout_ms: 120_000,
        stop: None,
    })));
}

#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    // FatalThrow would put a fatal screen over whatever game is running; just end this process.
    util::simple_panic_handler::<LmLogger>(info, abort::AbortLevel::ProcessExit())
}
