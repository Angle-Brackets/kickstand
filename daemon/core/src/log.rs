// SPDX-License-Identifier: GPL-3.0-only
//! Daemon log. Lines go to nxlink when the NRO build was started by nxlink, and are appended to
//! `/kickstand/logs/daemon.log` when [`enable_file`] has been called (the sysmodule has no other output).

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use nx::fs::{self, FileOpenOption};

pub const LOG_DIR: &str = "sdmc:/kickstand/logs";
pub const LOG_PATH: &str = "sdmc:/kickstand/logs/daemon.log";

/// The log is cut back to empty once it passes this size, so it can't fill the card.
const MAX_LOG_SIZE: usize = 1024 * 1024;

static FILE_ENABLED: AtomicBool = AtomicBool::new(false);

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
}

fn append_to_file(bytes: &[u8]) {
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
