// SPDX-License-Identifier: GPL-3.0-only
//! Opening hbmenu through a title without holding R: one program-specific entry in Atmosphère's
//! `override_config.ini` (see [`crate::path::OVERRIDE_CONFIG`]). Facts from Atmosphère's
//! `cfg_override.board.nintendo_nx.inc` and `config_templates/override_config.ini` (GPL-2.0; no code):
//! - `[hbl_config]` has eight slots, `program_id_N` and `override_key_N` (N in 0..=7; `program_id` and
//!   `override_key` are slot 0). An override key starting with `!` is on unless that key is held, so
//!   `override_key_N=!R` starts hbloader in that program's place unless R is held.
//! - Slot 0 defaults to the Album (`!R`); the others to program id 0, which nothing launches as.
//! - pm re-reads the file before every applet or application launch, but only sets what the file
//!   mentions: deleting a line leaves the value from the last read in place until a reboot. So the entry is
//!   never removed, only disarmed by setting its program id back to 0.
//!
//! The entry is three lines after the `[hbl_config]` header, found again by [`MARKER`].

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The comment line above Kickstand's entry.
pub const MARKER: &str = "; Kickstand: opens hbmenu through this title without R (program id 0 is off)";
/// The slots Kickstand may use, the last free one first. Slot 0 is the Album's.
const SLOTS: core::ops::RangeInclusive<u8> = 1..=7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// The user's own config uses every slot.
    NoFreeSlot,
}

/// Kickstand's entry in `ini`, if there is one: its slot and program id (0 when disarmed).
pub fn armed(ini: &str) -> Option<(u8, u64)> {
    let lines: Vec<&str> = ini.lines().collect();
    let at = lines.iter().position(|l| l.trim() == MARKER)?;
    let (key, value) = key_value(lines.get(at + 1)?)?;
    let slot = slot_of(key, "program_id")?;
    Some((slot, u64::from_str_radix(value, 16).ok()?))
}

/// `ini` with Kickstand's entry set to `program_id` (0 disarms it), creating the entry (and the
/// `[hbl_config]` section) if needed. Everything else is kept as it was.
pub fn set(ini: &str, program_id: u64) -> Result<String, ConfigError> {
    let newline = if ini.contains("\r\n") { "\r\n" } else { "\n" };
    let previous = armed(ini).map(|(slot, _)| slot);
    let mut lines: Vec<&str> = ini.lines().collect();
    if let Some(at) = lines.iter().position(|l| l.trim() == MARKER) {
        // The marker and the slot's lines under it.
        let mut end = at + 1;
        while end < lines.len() && end < at + 3 && previous.is_some_and(|s| is_slot_key(lines[end], s)) {
            end += 1;
        }
        lines.drain(at..end);
    }

    let used = |slot: u8| {
        let mut section = "";
        lines.iter().any(|line| {
            if let Some(name) = section_name(line) {
                section = name;
                return false;
            }
            section.eq_ignore_ascii_case("hbl_config") && is_slot_key(line, slot)
        })
    };
    let slot = match previous.filter(|&s| !used(s)) {
        Some(slot) => slot,
        None => SLOTS.rev().find(|&s| !used(s)).ok_or(ConfigError::NoFreeSlot)?,
    };
    let entry = [
        String::from(MARKER),
        format!("program_id_{slot}={program_id:016x}"),
        format!("override_key_{slot}=!R"),
    ];

    let mut out: Vec<String> = lines.iter().map(|l| String::from(*l)).collect();
    match lines.iter().position(|l| section_name(l).is_some_and(|n| n.eq_ignore_ascii_case("hbl_config"))) {
        Some(header) => {
            out.splice(header + 1..header + 1, entry);
        }
        None => {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push(String::from("[hbl_config]"));
            out.extend(entry);
        }
    }
    let mut text = out.join(newline);
    text.push_str(newline);
    Ok(text)
}

fn section_name(line: &str) -> Option<&str> {
    line.trim().strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

fn key_value(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.starts_with(';') || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim()))
}

/// Which slot `key` (`<prefix>_N`, or plain `<prefix>` for slot 0) is for.
fn slot_of(key: &str, prefix: &str) -> Option<u8> {
    let head = key.get(..prefix.len()).filter(|h| h.eq_ignore_ascii_case(prefix))?;
    match &key[head.len()..] {
        "" => Some(0),
        rest => rest.strip_prefix('_')?.parse().ok().filter(|n| *n <= 7),
    }
}

/// Whether `line` sets something of slot `slot` (its program id, key or address space).
fn is_slot_key(line: &str, slot: u8) -> bool {
    key_value(line).is_some_and(|(key, _)| {
        ["program_id", "override_key", "override_address_space"].iter().any(|p| slot_of(key, p) == Some(slot))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = "[hbl_config]\n; program_id_0=010000000000100D\n; override_key_0=!R\n\n[default_config]\n; override_key=!L\n";

    #[test]
    fn arms_and_disarms_in_place() {
        let armed_ini = set(TEMPLATE, 0x0100_6FE0_1347_2000).unwrap();
        assert_eq!(
            armed_ini,
            format!("[hbl_config]\n{MARKER}\nprogram_id_7=01006fe013472000\noverride_key_7=!R\n; program_id_0=010000000000100D\n; override_key_0=!R\n\n[default_config]\n; override_key=!L\n")
        );
        assert_eq!(armed(&armed_ini), Some((7, 0x0100_6FE0_1347_2000)));
        let off = set(&armed_ini, 0).unwrap();
        assert_eq!(armed(&off), Some((7, 0)));
        assert_eq!(off.lines().count(), armed_ini.lines().count());
        assert_eq!(set(&off, 0).unwrap(), off);
    }

    #[test]
    fn creates_the_section_and_keeps_the_users_slots() {
        assert_eq!(set("", 5).unwrap(), format!("[hbl_config]\n{MARKER}\nprogram_id_7=0000000000000005\noverride_key_7=!R\n"));
        let mine = "[default_config]\r\noverride_key=!L\r\n";
        assert!(set(mine, 5).unwrap().ends_with(&format!("\r\n\r\n[hbl_config]\r\n{MARKER}\r\nprogram_id_7=0000000000000005\r\noverride_key_7=!R\r\n")));

        let busy = "[hbl_config]\nprogram_id_7=0100000000001000\noverride_key_7=R\noverride_address_space_6=36_bit\n";
        let ini = set(busy, 5).unwrap();
        assert_eq!(armed(&ini), Some((5, 5)));
        assert!(ini.contains("program_id_7=0100000000001000\noverride_key_7=R\n"));
        // Slot 7 in another section doesn't count.
        assert_eq!(armed(&set("[default_config]\nprogram_id_7=1\n", 5).unwrap()), Some((7, 5)));

        let full: String = (1..=7).map(|n| format!("program_id_{n}=1\n")).collect();
        assert_eq!(set(&format!("[hbl_config]\n{full}"), 5), Err(ConfigError::NoFreeSlot));
    }

    #[test]
    fn moves_when_the_user_takes_its_slot() {
        let ini = set(TEMPLATE, 5).unwrap().replace("; program_id_0=010000000000100D", "program_id_7=0100000000001000");
        let moved = set(&ini, 6).unwrap();
        assert_eq!(armed(&moved), Some((6, 6)));
        assert_eq!(moved.matches(MARKER).count(), 1);
        assert!(moved.contains("program_id_7=0100000000001000"));
    }
}
