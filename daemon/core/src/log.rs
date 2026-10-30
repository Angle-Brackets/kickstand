// SPDX-License-Identifier: GPL-3.0-only
//! Daemon log. Lines go to nxlink when the NRO build was started by nxlink, and are appended to
//! `/kickstand/logs/daemon.log` when [`enable_file`] has been called (the sysmodule has no other output),
//! and are kept for an on-screen view after [`enable_recent`].

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use nx::fs::{self, FileOpenOption};

pub const LOG_DIR: &str = "sdmc:/kickstand/logs";
pub const LOG_PATH: &str = "sdmc:/kickstand/logs/daemon.log";

/// The log is cut back to empty once it passes this size, so it can't fill the card.
const MAX_LOG_SIZE: usize = 1024 * 1024;

static FILE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Lines not yet taken by [`take_recent`], capped so an unread backlog can't grow without bound.
static RECENT: nx::sync::Mutex<Option<VecDeque<String>>> = nx::sync::Mutex::new(None);
const RECENT_MAX: usize = 200;

/// Starts keeping log lines for [`take_recent`] (the NRO's screen).
pub fn enable_recent() {
    *RECENT.lock() = Some(VecDeque::new());
}

/// The lines logged since the last call, oldest first.
pub fn take_recent() -> Vec<String> {
    RECENT.lock().as_mut().map_or_else(Vec::new, |q| q.drain(..).collect())
}

/// Starts appending to [`LOG_PATH`]. The SD card must be mounted as `sdmc`.
pub fn enable_file() {
    let _ = fs::create_directory("sdmc:/kickstand");
    let _ = fs::create_directory(LOG_DIR);
    FILE_ENABLED.store(true, Ordering::Relaxed);
}

#[doc(hidden)]
pub fn _log(args: core::fmt::Arguments) {
    let mut line = String::new();
    let ms = crate::uptime_ms();
    let _ = writeln!(line, "[{}.{:03}] {}", ms / 1000, ms % 1000, args);
    nxlink::print!("{line}");
    if FILE_ENABLED.load(Ordering::Relaxed) {
        append_to_file(line.as_bytes());
    }
    if let Some(q) = RECENT.lock().as_mut() {
        if q.len() == RECENT_MAX {
            q.pop_front();
        }
        q.push_back(line);
    }
}

/// Held while appending, so lines from different threads don't land at the same offset.
static FILE_LOCK: nx::sync::Mutex<()> = nx::sync::Mutex::new(());

fn append_to_file(bytes: &[u8]) {
    let _one_writer = FILE_LOCK.lock();
    let Ok(mut file) = fs::open_file(LOG_PATH, FileOpenOption::Create() | FileOpenOption::Write() | FileOpenOption::Append()) else {
        return;
    };
    if file.get_size().unwrap_or(0) > MAX_LOG_SIZE {
        drop(file);
        let _ = fs::remove_file(LOG_PATH);
        let Ok(new_file) = fs::open_file(LOG_PATH, FileOpenOption::Create() | FileOpenOption::Write() | FileOpenOption::Append()) else {
            return;
        };
        file = new_file;
    }
    let _ = file.write_array::<u8, true>(bytes);
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::_log(format_args!($($arg)*)) };
}
