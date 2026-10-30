// SPDX-License-Identifier: GPL-3.0-only
//! Fatal errors since the daemon started. A fatal error (a crashed home menu, for one) leaves the console on
//! Atmosphère's error screen until it reboots, but sysmodules keep running, so the daemon can still say what
//! happened and reboot it. Atmosphère writes a report to [`FATAL_REPORTS`] for each, named
//! `<timestamp>_<program id>.log`; one newer than any present at start is from this boot.

use alloc::string::String;
use alloc::vec::Vec;

use proto::DirEntry;

use crate::sd;

pub const FATAL_REPORTS: &str = "/atmosphere/fatal_reports";

/// The newest report name when the daemon started; `None` until [`note_start`] ran.
static NEWEST_AT_START: nx::sync::Mutex<Option<String>> = nx::sync::Mutex::new(None);

fn reports() -> Vec<DirEntry> {
    // The folder doesn't exist until the first fatal error.
    let entries = sd::list_dir(FATAL_REPORTS, 1024).map(|(e, _)| e).unwrap_or_default();
    entries.into_iter().filter(|e| !e.is_dir && e.name.ends_with(".log")).collect()
}

/// Notes which reports already exist. Call once at start, before serving clients.
pub fn note_start() {
    let newest = reports().into_iter().map(|e| e.name).max().unwrap_or_default();
    *NEWEST_AT_START.lock() = Some(newest);
}

/// Reports written since [`note_start`], oldest first. The timestamps in the names have the same width, so
/// they sort as text.
pub fn since_start() -> Vec<DirEntry> {
    let Some(newest) = NEWEST_AT_START.lock().clone() else { return Vec::new() };
    let mut new: Vec<DirEntry> = reports().into_iter().filter(|e| e.name > newest).collect();
    new.sort_by(|a, b| a.name.cmp(&b.name));
    new
}
