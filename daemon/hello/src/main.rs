// SPDX-License-Identifier: GPL-3.0-only
//! Prints a few lines over nxlink and exits back to hbmenu. Run with
//! `nxlink -s -a <switch-ip> hello.nro`; it proves the toolchain, the nx runtime and sockets work.

#![no_std]
#![no_main]

use nx::diag::abort;
use nx::diag::log::lm::LmLogger;
use nx::{svc, util, version};
use nxlink::println;

nx::rrt0_define_module_name!("switch-mcp-hello");
nx::rrt0_initialize_heap!();

#[unsafe(no_mangle)]
fn main() {
    nx::socket::initialize(
        nx::socket::BsdSrvkind::User,
        Default::default(),
        None,
        nx::socket::Paralellism::One,
    )
    .unwrap();
    // Without nxlink there is nowhere to print, so this just exits.
    if !nxlink::connect().unwrap_or(false) {
        return;
    }

    println!("Hello from Rust on the Switch!");
    println!("firmware {}", version::get_version());
    println!("loader: {}", nx::hbl::get_loader_info());
    for (i, arg) in nxlink::args().enumerate() {
        println!("arg {i}: {arg}");
    }
    let ticks = nx::arm::get_system_tick();
    println!("uptime {} s", ticks / nx::arm::get_system_tick_frequency());
    // Give nxlink a moment to drain the socket before the process (and its sockets) go away.
    let _ = svc::sleep_thread(200_000_000);
    nxlink::disconnect();
}

#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    util::simple_panic_handler::<LmLogger>(info, abort::AbortLevel::FatalThrow())
}
