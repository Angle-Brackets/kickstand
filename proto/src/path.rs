// SPDX-License-Identifier: GPL-3.0-only
//! SD card path rules, shared so the MCP server can reject a bad path before it reaches the console.
//!
//! Paths are absolute from the SD root (`/switch/paperboat/paperboat.nro`); the daemon adds `sdmc:`.
//! The SD card is FAT/exFAT, so every comparison here ignores ASCII case.

use alloc::string::String;

/// The directories uploads may write into. Everything else on the card is read-only to the daemon.
/// `/switch/ttydrecomp/` is reserved for TTYDRecomp; that port has no SD layout yet. The last two are
/// the daemon's own NRO folder (listed in hbmenu) and its sysmodule, so both can be updated over the
/// network.
pub const WRITE_ROOTS: &[&str] = &[
    "/switch/paperboat/",
    "/switch/ttydrecomp/",
    "/kickstand/",
    "/switch/kickstand/",
    "/atmosphere/contents/42000000004D4350/",
];

/// The payload Atmosphère reboots into; on this setup, hekate.
pub const REBOOT_PAYLOAD: &str = "/atmosphere/reboot_payload.bin";
/// hekate's boot entries, read to turn an entry name into the index hekate wants.
pub const HEKATE_INI: &str = "/bootloader/hekate_ipl.ini";

/// Single files outside the write roots that are also ours: where hbmenu's NetLoader saved our NROs
/// before `/switch/kickstand/` existed. Writable so they can be cleaned up.
const OWN_FILES: &[&str] = &["/switch/kickstand.nro", "/switch/kickstand-hello.nro"];

/// Atmosphère's override config. Not under a write root: only [`crate::Request::HblOverride`] changes it,
/// and only Kickstand's own entry ([`crate::hbl`]).
pub const OVERRIDE_CONFIG: &str = "/atmosphere/config/override_config.ini";

/// Where the NRO daemon is installed for launching from hbmenu.
pub const NRO_PATH: &str = "/switch/kickstand/kickstand.nro";

/// The sysmodule's program id (ends in ASCII "MCP"), and where Atmosphère loads it from.
pub const SYSMODULE_PROGRAM_ID: u64 = 0x4200_0000_004D_4350;
pub const SYSMODULE_NSP: &str = "/atmosphere/contents/42000000004D4350/exefs.nsp";
/// Present means Atmosphère starts the sysmodule at boot. Creating it needs the user's go-ahead.
pub const SYSMODULE_BOOT2_FLAG: &str = "/atmosphere/contents/42000000004D4350/flags/boot2.flag";

/// Where uploads are staged before being moved into place.
pub const STAGING_DIR: &str = "/kickstand/staging";

/// Left by a reboot the agent asked for; the sysmodule removes it at the next boot and gets past the lock
/// screen. A boot without it (the user's own) is left alone.
pub const AGENT_REBOOT_FLAG: &str = "/kickstand/agent-reboot.flag";

/// The tokens the daemon accepts, one per line (blank lines and `#` comments ignored): written by hand
/// or by pairing ([`crate::Request::Pair`]).
pub const TOKEN_PATH: &str = "/kickstand/token.txt";

/// Top-level directories that are never read: NAND images and backups, and Nintendo's own content.
const DENIED_ROOTS: &[&str] = &["/emummc", "/backup", "/nintendo", "/atmosphere/automatic_backups"];

/// Longest path the daemon accepts, leaving room for `sdmc:` within the 0x301-byte FS path limit.
pub const MAX_PATH: usize = 0x2F0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    NotAbsolute,
    TooLong,
    /// A `..` component, or a character FAT or the FS service rejects (`:`, `\`, control bytes).
    BadComponent,
    /// Reading this path could expose keys, NAND backups or console-unique data.
    ReadDenied,
    /// Writes are limited to [`WRITE_ROOTS`].
    WriteDenied,
}

impl PathError {
    pub fn describe(self) -> &'static str {
        match self {
            PathError::NotAbsolute => "path must start with / (relative to the SD card root)",
            PathError::TooLong => "path is too long",
            PathError::BadComponent => "path contains '..', ':', '\\' or a control character",
            PathError::ReadDenied => "reading keys, NAND backups or /Nintendo is not allowed",
            PathError::WriteDenied => {
                "writes and deletes are limited to /switch/paperboat/, /switch/ttydrecomp/, and the daemon's \
                 own /kickstand/, /switch/kickstand/ and /atmosphere/contents/42000000004D4350/"
            }
        }
    }
}

/// Returns `path` with `.` and empty components removed, or an error if it can't be made safe.
/// The root normalizes to `/`.
pub fn normalize(path: &str) -> Result<String, PathError> {
    if !path.starts_with('/') {
        return Err(PathError::NotAbsolute);
    }
    let mut out = String::with_capacity(path.len());
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.chars().any(|c| c == ':' || c == '\\' || c.is_control()) {
            return Err(PathError::BadComponent);
        }
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    if out.len() > MAX_PATH {
        return Err(PathError::TooLong);
    }
    Ok(out)
}

/// Normalizes `path` and checks that it may be read or listed.
pub fn check_read(path: &str) -> Result<String, PathError> {
    let path = normalize(path)?;
    let denied_root = DENIED_ROOTS.iter().any(|root| under(&path, root));
    if denied_root || is_key_file(&path) {
        return Err(PathError::ReadDenied);
    }
    Ok(path)
}

/// Normalizes `path` and checks that it may be written or deleted. The path must be inside a write
/// root, not the root itself.
pub fn check_write(path: &str) -> Result<String, PathError> {
    let path = normalize(path)?;
    let inside_root = WRITE_ROOTS
        .iter()
        .any(|root| path.len() > root.len() && starts_with_ignore_case(&path, root))
        || OWN_FILES.iter().any(|f| path.eq_ignore_ascii_case(f));
    if !inside_root || is_key_file(&path) {
        return Err(PathError::WriteDenied);
    }
    Ok(path)
}

/// The directory part of a normalized path (`/a/b` -> `/a`, `/a` -> `/`).
pub fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

/// Whether normalized `path` is `root` or inside it.
fn under(path: &str, root: &str) -> bool {
    starts_with_ignore_case(path, root)
        && (path.len() == root.len() || path.as_bytes()[root.len()] == b'/')
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn is_key_file(path: &str) -> bool {
    let name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
    let lower = name.as_bytes();
    lower.len() >= 5 && lower[lower.len() - 5..].eq_ignore_ascii_case(b".keys")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize("/").unwrap(), "/");
        assert_eq!(normalize("//switch/./paperboat//").unwrap(), "/switch/paperboat");
        assert_eq!(normalize("switch"), Err(PathError::NotAbsolute));
        assert_eq!(normalize("/switch/../atmosphere"), Err(PathError::BadComponent));
        assert_eq!(normalize("/sdmc:/x"), Err(PathError::BadComponent));
        assert_eq!(normalize("/a\\b"), Err(PathError::BadComponent));
    }

    #[test]
    fn read_rules() {
        assert!(check_read("/").is_ok());
        assert!(check_read("/atmosphere/crash_reports").is_ok());
        assert!(check_read("/Nintendo2/x").is_ok());
        assert_eq!(check_read("/switch/prod.keys"), Err(PathError::ReadDenied));
        assert_eq!(check_read("/switch/PROD.KEYS"), Err(PathError::ReadDenied));
        assert_eq!(check_read("/emuMMC/RAW1"), Err(PathError::ReadDenied));
        assert_eq!(check_read("/Nintendo"), Err(PathError::ReadDenied));
        assert_eq!(check_read("/atmosphere/automatic_backups/x"), Err(PathError::ReadDenied));
    }

    #[test]
    fn write_rules() {
        assert!(check_write("/switch/paperboat/paperboat.nro").is_ok());
        assert!(check_write("/Switch/PaperBoat/x.nro").is_ok());
        assert!(check_write("/kickstand/staging/1").is_ok());
        assert_eq!(check_write("/switch/paperboat"), Err(PathError::WriteDenied));
        assert_eq!(check_write("/switch/paperboat2/x"), Err(PathError::WriteDenied));
        assert_eq!(check_write("/atmosphere/contents/x"), Err(PathError::WriteDenied));
        assert!(check_write(SYSMODULE_NSP).is_ok());
        assert!(check_write("/switch/kickstand.nro").is_ok());
        assert_eq!(check_write("/switch/kickstand.nro2"), Err(PathError::WriteDenied));
        assert_eq!(check_write("/switch/hbmenu.nro"), Err(PathError::WriteDenied));
        assert_eq!(check_write("/atmosphere/contents/0100000000000000/exefs.nsp"), Err(PathError::WriteDenied));
        assert_eq!(check_write("/kickstand/prod.keys"), Err(PathError::WriteDenied));
    }

    #[test]
    fn parents() {
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/a"), "/");
    }
}
