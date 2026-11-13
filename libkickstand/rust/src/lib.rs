// SPDX-License-Identifier: GPL-3.0-only
//! libkickstand for Rust homebrew built on the [nx](https://github.com/aarch64-switch-rs/nx) crate: makes
//! Kickstand's `reload_nro` fast and safe, as the C library does for libnx apps. hbloader loads the next
//! build into this same process, so:
//!
//! 1. **Quit on request.** Call [`quit_requested`] once a frame and quit when it's true: Kickstand asks
//!    instead of pressing buttons, so apps that only quit from a menu reload too.
//! 2. **The next-load hook.** [`finish`], just before `main` returns, claims Kickstand's request and has
//!    hbloader load the new build instead of hbmenu.
//! 3. **No threads left running.** A thread still running when `main` returns would go on running the old
//!    build's code after the new one loads over it. Start threads with [`spawn`]; [`finish`] gives them a
//!    second to end, then pauses the rest for good (Horizon can't end another thread). nx keeps thread stacks
//!    in the heap, so unlike libnx a finished thread leaves nothing behind.
//!
//! Quitting on request and the hook only act when the app was started from a PC over nxlink, so a launch from
//! hbmenu by hand behaves as before. JIT code memory the app made itself (`svcCreateCodeMemory`) must still be
//! unmapped by the app before it returns.
//!
//! ```ignore
//! let worker = kickstand::spawn(nx::thread::Builder::new(), || audio_loop())?;
//! while !kickstand::quit_requested() && !plus_pressed() {
//!     frame();
//! }
//! let done = kickstand::finish();
//! ```

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nx::fs::{self, FileOpenOption};
use nx::svc;
use nx::sync::Mutex;
use nx::thread::{Builder, JoinHandle};
use nx::util::AsInner;

/// The request Kickstand leaves: the NRO to load, then its argv, one per line.
const REQUEST: &str = "sdmc:/kickstand/next-load";
/// Kickstand looks for this in an NRO to know it can ask the app to quit instead of pressing buttons.
static MARKER: [u8; 41] = *b"kickstand-lib/1 sdmc:/kickstand/next-load";

static QUIT: AtomicBool = AtomicBool::new(false);
static LAST_CHECK: AtomicU64 = AtomicU64::new(0);
static FS_TRIED: AtomicBool = AtomicBool::new(false);
static THREADS: Mutex<Vec<Tracked>> = Mutex::new(Vec::new());

struct Tracked {
    handle: svc::Handle,
    done: Arc<AtomicBool>,
}

/// Whether Kickstand asked this app to quit for a reload: checked at most 5 times a second, true from then
/// on. Always false unless the app was started from a PC over nxlink.
pub fn quit_requested() -> bool {
    if QUIT.load(Ordering::Relaxed) {
        return true;
    }
    if !from_pc() {
        return false;
    }
    let now = nx::arm::get_system_tick();
    let last = LAST_CHECK.load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < nx::arm::get_system_tick_frequency() / 5 {
        return false;
    }
    LAST_CHECK.store(now, Ordering::Relaxed);
    sd_card();
    if fs::get_entry_type(REQUEST).is_ok() {
        QUIT.store(true, Ordering::Relaxed);
    }
    QUIT.load(Ordering::Relaxed)
}

/// [`Builder::spawn`], keeping track of the thread so [`finish`] can make sure it isn't left running.
pub fn spawn<F, T>(builder: Builder, f: F) -> nx::result::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    /// Marks the thread done however `f` ends, a panic included.
    struct Done(Arc<AtomicBool>);
    impl Drop for Done {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let done = Arc::new(AtomicBool::new(false));
    let mark = Done(done.clone());
    let thread = builder.spawn(move || {
        let _mark = mark;
        f()
    })?;
    THREADS.lock().push(Tracked { handle: thread.as_inner().handle(), done });
    Ok(thread)
}

/// What [`finish`] did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Finished {
    /// The request was claimed: hbloader loads the new build next.
    pub reloading: bool,
    /// Threads from [`spawn`] that had ended, or ended within the second they were given.
    pub ended: usize,
    /// Threads still running after that, now paused for good.
    pub paused: usize,
}

/// Call just before `main` returns: claims Kickstand's reload request, if there is one, so hbloader loads
/// the new build next, and makes sure no thread from [`spawn`] is left running.
pub fn finish() -> Finished {
    // Keeps the marker in the binary.
    core::hint::black_box(&MARKER);
    let mut finished = Finished { reloading: claim(), ..Finished::default() };
    let threads = core::mem::take(&mut *THREADS.lock());
    let freq = nx::arm::get_system_tick_frequency();
    let start = nx::arm::get_system_tick();
    while threads.iter().any(|t| !t.done.load(Ordering::Acquire)) && nx::arm::get_system_tick() - start < freq {
        let _ = svc::sleep_thread(10_000_000);
    }
    for thread in &threads {
        if thread.done.load(Ordering::Acquire) {
            finished.ended += 1;
        } else if svc::set_thread_activity(thread.handle, svc::SchedulerState::Paused).is_ok() {
            finished.paused += 1;
        }
    }
    finished
}

/// Claims the request (the delete decides: Kickstand withdraws one it gave up on the same way) and hands it
/// to hbloader.
fn claim() -> bool {
    if !from_pc() {
        return false;
    }
    sd_card();
    let Ok(mut file) = fs::open_file(REQUEST, FileOpenOption::Read()) else { return false };
    let mut buf = [0u8; 2600];
    let mut len = 0;
    while let Ok(n @ 1..) = file.read_array(&mut buf[len..]) {
        len += n;
    }
    drop(file);
    let mut lines = core::str::from_utf8(&buf[..len]).unwrap_or("").lines();
    let (Some(path), Some(argv)) = (lines.next(), lines.next()) else { return false };
    if fs::remove_file(REQUEST).is_err() {
        return false;
    }
    // nx copies both into hbloader's buffers; it just wants 'static strings.
    let (path, argv) = (String::leak(path.into()), String::leak(argv.into()));
    nx::hbl::set_next_load_entry(path, argv).is_ok()
}

/// Started from a PC: nxlink's address marker is the last argument.
fn from_pc() -> bool {
    nx::hbl::get_argv().is_some_and(|argv| argv.contains("_NXLINK_"))
}

/// The SD card at `sdmc:`, if the app hasn't mounted it itself (errors mean it already is, or can't be).
fn sd_card() {
    if !FS_TRIED.swap(true, Ordering::Relaxed) {
        let _ = fs::initialize_fspsrv_session();
        let _ = fs::mount_sd_card("sdmc");
    }
}
