// SPDX-License-Identifier: GPL-3.0-only
//! Prints a few lines over nxlink and exits back to hbmenu (or with `--hang`/`--crash`, tests the watchdog). Run with
//! `nxlink -s -a <switch-ip> hello.nro`; it proves the toolchain, the nx runtime and sockets work.
//!
//! With `--stay` it keeps running until + is pressed, and it is the reference for Kickstand's hot-reload hook
//! ([`kickstand_next_load`]): `kickstand reload hello.nro -- --stay` swaps in a new build in about a second.

#![no_std]
#![no_main]

extern crate alloc;

use nx::diag::abort;
use nx::diag::log::lm::LmLogger;
use nx::fs::{self, FileOpenOption};
use nx::service::hid;
use nx::{input, svc, util, version};
use nxlink::println;

nx::rrt0_define_module_name!("kickstand-hello");
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
    println!("argv: {:?}, nxlink host: {:?}", nx::hbl::get_argv(), nxlink::host());
    for (i, arg) in nxlink::args().enumerate() {
        println!("arg {i}: {arg}");
    }
    // For testing the watchdog: a hang (main thread asleep behind a still screen) or a crash (a data abort,
    // which gets a crash report; a panic here would be a console fatal).
    if nxlink::args().any(|a| a == "--work") {
        println!("running CPU workload for flamegraph profiling (10 s)...");
        run_workload(10_000);
    }
    if nxlink::args().any(|a| a == "--hang") {
        println!("hanging: sleeping forever");
        loop {
            let _ = svc::sleep_thread(1_000_000_000);
        }
    }
    if nxlink::args().any(|a| a == "--crash") {
        println!("crashing: writing through a null pointer");
        let _ = svc::sleep_thread(200_000_000);
        unsafe { core::ptr::null_mut::<u32>().write_volatile(0xDEAD) };
    }
    let ticks = nx::arm::get_system_tick();
    println!("uptime {} s", ticks / nx::arm::get_system_tick_frequency());
    if nxlink::args().any(|a| a == "--stay") {
        // Set at build time to tell builds apart when hot-reloading (`HELLO_BUILD=2 cargo nx build ...`).
        println!("build {}; press + to exit", option_env!("HELLO_BUILD").unwrap_or("dev"));
        wait_for_plus();
        println!("exiting");
    }
    kickstand_next_load();
    // Give nxlink a moment to drain the socket before the process (and its sockets) go away.
    let _ = svc::sleep_thread(100_000_000);
    nxlink::disconnect();
}

/// Kickstand's hot-reload hook, called just before returning from `main`. If Kickstand left a next-load
/// request, claim it by deleting it and have hbloader load that NRO next, instead of hbmenu. The request is
/// two lines: the NRO's path, then its argv string, both passed to `envSetNextLoad` as they are.
/// - Only when started from a PC over nxlink, so a leftover request can't redirect a launch from hbmenu.
/// - Only if *our* delete worked: Kickstand withdraws a request it gave up on by deleting it, so whichever
///   delete succeeds decides, and Kickstand knows which way it went.
fn kickstand_next_load() {
    const REQUEST: &str = "sdmc:/kickstand/next-load";
    if nxlink::host().is_none() || fs::initialize_fspsrv_session().is_err() || fs::mount_sd_card("sdmc").is_err() {
        return;
    }
    let Ok(mut file) = fs::open_file(REQUEST, FileOpenOption::Read()) else {
        return;
    };
    let mut buf = [0u8; 2600];
    let mut len = 0;
    while let Ok(n @ 1..) = file.read_array(&mut buf[len..]) {
        len += n;
    }
    drop(file);
    let mut lines = core::str::from_utf8(&buf[..len]).unwrap_or("").lines();
    if let (Some(path), Some(argv)) = (lines.next(), lines.next())
        && fs::remove_file(REQUEST).is_ok()
    {
        // nx copies both into hbloader's buffers; its signature just wants 'static strings.
        let (path, argv) = (alloc::string::String::leak(path.into()), alloc::string::String::leak(argv.into()));
        let _ = nx::hbl::set_next_load_entry(path, argv);
    }
}

/// Waits until + is pressed on the handheld or the first controller.
fn wait_for_plus() {
    let styles = hid::NpadStyleTag::FullKey() | hid::NpadStyleTag::Handheld() | hid::NpadStyleTag::JoyDual();
    let Ok(ctx) = input::Context::new(styles, 1) else {
        return;
    };
    let mut players = [ctx.get_player(hid::NpadIdType::No1), ctx.get_player(hid::NpadIdType::Handheld)];
    loop {
        if players.iter_mut().any(|p| p.get_buttons_down().contains(hid::NpadButton::Plus())) {
            return;
        }
        let _ = svc::sleep_thread(16_000_000);
    }
}

#[inline(never)]
fn run_workload(duration_ms: u64) {
    let freq = nx::arm::get_system_tick_frequency();
    let start = nx::arm::get_system_tick();
    let deadline = start + (freq * duration_ms) / 1000;
    let mut seed = 0x1234_5678_9ABC_DEF0u64;
    while nx::arm::get_system_tick() < deadline {
        seed = simulate_frame(seed);
        core::hint::black_box(seed);
    }
}

#[inline(never)]
fn simulate_frame(mut state: u64) -> u64 {
    state = rasterize_scene(state);
    state = step_rigid_bodies(state);
    state = mix_audio_voices(state);
    core::hint::black_box(state)
}

#[inline(never)]
fn rasterize_scene(mut state: u64) -> u64 {
    for i in 0..3_200u64 {
        state = shade_tile_fragment(state ^ i);
    }
    core::hint::black_box(state)
}

#[inline(never)]
fn shade_tile_fragment(x: u64) -> u64 {
    let mut v = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for _ in 0..16 {
        v = mix_step(v.rotate_left(13), 0xBF58_476D_1CE4_E5B9);
    }
    core::hint::black_box(v)
}

#[inline(never)]
fn step_rigid_bodies(mut state: u64) -> u64 {
    for i in 0..1_800u64 {
        state = solve_contact_constraints(state.wrapping_add(i));
    }
    core::hint::black_box(state)
}

#[inline(never)]
fn solve_contact_constraints(x: u64) -> u64 {
    let mut v = x ^ 0x94D0_49BB_1331_11EB;
    for _ in 0..14 {
        v = mix_step(v.wrapping_mul(0xD6E8_FEB8_6659_FD93), 0x1331_11EB_D6E8_FEB8);
    }
    core::hint::black_box(v)
}

#[inline(never)]
fn mix_audio_voices(mut state: u64) -> u64 {
    for i in 0..900u64 {
        state = biquad_filter_pass(state ^ (i << 7));
    }
    core::hint::black_box(state)
}

#[inline(never)]
fn biquad_filter_pass(x: u64) -> u64 {
    let mut v = x.wrapping_add(0x2545_F491_4F6C_DD1D);
    for _ in 0..12 {
        v = mix_step(v ^ (v >> 15), 0x85EB_CA6B_C2B2_AE35);
    }
    core::hint::black_box(v)
}

#[inline(never)]
fn mix_step(a: u64, b: u64) -> u64 {
    core::hint::black_box(a.wrapping_add(b).rotate_left(7))
}

#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo) -> ! {
    util::simple_panic_handler::<LmLogger>(info, abort::AbortLevel::FatalThrow())
}
