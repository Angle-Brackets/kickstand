// SPDX-License-Identifier: GPL-3.0-only
//! Kickstand's entry in Atmosphère's `override_config.ini` ([`proto::hbl`]): armed for one launch so a
//! title starts as hbloader without R held, disarmed straight after by the client, or by the daemon itself
//! after [`ARM_MS`] if the client never does (it died or was killed mid-launch). Left armed, the title would
//! start as hbloader unless R is held: the reverse of what holding R does for every other title.

use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

use proto::path::OVERRIDE_CONFIG;
use proto::{Error, ErrorKind};

use crate::{log, sd};

/// Larger than any override config a person writes; a bigger file is left alone.
const MAX_INI: usize = 64 * 1024;
/// The new config is written here, then moved into place, so the user's settings survive a failed write.
const STAGED: &str = "/atmosphere/config/override_config.ini.kickstand";

/// How long an armed entry may stay armed. A launch disarms it within ~15 s (launch.rs `try_tile`).
pub const ARM_MS: u64 = 60_000;
/// When the armed entry is due to be disarmed (uptime ms), or 0 when it isn't armed.
static ARMED_UNTIL: AtomicU64 = AtomicU64::new(0);
/// Client threads and the expiry thread both edit the file.
static LOCK: nx::sync::Mutex<()> = nx::sync::Mutex::new(());

/// Points Kickstand's entry at `program_id` (0 disarms it). Disarming a config without an armed entry
/// changes nothing; arming creates the file if there is none.
pub fn set(program_id: u64) -> Result<(), Error> {
    let _guard = LOCK.lock();
    let result = write(program_id);
    if program_id == 0 {
        if result.is_ok() {
            ARMED_UNTIL.store(0, Ordering::Relaxed);
        }
    } else {
        // Even if the write failed part way, check again later.
        ARMED_UNTIL.store(crate::uptime_ms() + ARM_MS, Ordering::Relaxed);
    }
    result
}

/// Disarms an entry armed more than [`ARM_MS`] ago. Called often (the sysmodule's expiry thread).
pub fn expire(now_ms: u64) {
    let until = ARMED_UNTIL.load(Ordering::Relaxed);
    if until == 0 || now_ms < until {
        return;
    }
    log!("hbl override: armed for {} s without being disarmed; disarming", ARM_MS / 1000);
    if let Err(e) = set(0) {
        log!("couldn't disarm the hbl override: {}", e.message);
        // Don't retry ten times a second.
        ARMED_UNTIL.store(now_ms + ARM_MS, Ordering::Relaxed);
    }
}

fn write(program_id: u64) -> Result<(), Error> {
    let ini = match sd::read(OVERRIDE_CONFIG, 0, MAX_INI + 1) {
        Ok((_, size)) if size as usize > MAX_INI => {
            return Err(error(format!("{OVERRIDE_CONFIG} is over {MAX_INI} bytes; left alone")));
        }
        Ok((data, _)) => String::from_utf8(data).map_err(|_| error(format!("{OVERRIDE_CONFIG} isn't UTF-8; left alone")))?,
        Err(e) if e.kind == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if program_id == 0 && proto::hbl::armed(&ini).is_none_or(|(_, id)| id == 0) {
        return Ok(());
    }
    let text = proto::hbl::set(&ini, program_id)
        .map_err(|_| error(format!("{OVERRIDE_CONFIG} uses every [hbl_config] slot; free one for Kickstand")))?;
    if text == ini {
        return Ok(());
    }
    sd::create_dirs("/atmosphere/config")?;
    let mut file = sd::create_for_write(STAGED, text.len() as u64)?;
    file.write_array::<u8, true>(text.as_bytes()).map_err(|rc| sd::io_error("write", STAGED, rc))?;
    drop(file);
    sd::remove_if_exists(OVERRIDE_CONFIG)?;
    sd::rename(STAGED, OVERRIDE_CONFIG)?;
    log!("hbl override: {}", if program_id == 0 { String::from("off") } else { format!("{program_id:016x}") });
    Ok(())
}

/// At start: disarms an entry a launch left armed (the daemon stopped before disarming it), so the title
/// starts as itself again.
pub fn disarm_leftover() {
    if let Err(e) = set(0) {
        log!("couldn't disarm the hbl override: {}", e.message);
    }
}

fn error(message: String) -> Error {
    Error { kind: ErrorKind::Io, message, result: None }
}
