// SPDX-License-Identifier: GPL-3.0-only
//! Where the Switch's address and the shared token come from.
//!
//! - host: `--host`, then `$SWITCH_IP`, then `~/.config/kickstand/host`
//! - token: `$KICKSTAND_TOKEN`, then `~/.config/kickstand/token` (created by `kickstand pair` or `kickstand token`)
//! - host title (the title Atmosphère starts hbmenu in place of, with full memory): `~/.config/kickstand/host-title`,
//!   written by `kickstand hbmenu` as `<hex id> <name>`

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("kickstand"));
    }
    if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA").filter(|d| !d.is_empty()).context("APPDATA is not set")?;
        return Ok(PathBuf::from(appdata).join("kickstand"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/kickstand"))
}

pub fn host(arg: Option<String>) -> Result<String> {
    if let Some(h) = arg.or_else(|| std::env::var("SWITCH_IP").ok()).filter(|h| !h.is_empty()) {
        return Ok(h);
    }
    let path = config_dir()?.join("host");
    match std::fs::read_to_string(&path) {
        Ok(h) if !h.trim().is_empty() => Ok(h.trim().to_string()),
        _ => bail!("no Switch address: pass --host, set SWITCH_IP, or write it to {}", path.display()),
    }
}

/// Writes `text` to the config file `name`, making the folder first.
fn save(name: &str, text: &str) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(name), text)?;
    Ok(())
}

/// Makes `host` the console to use (`~/.config/kickstand/host`).
pub fn save_host(host: &str) -> Result<()> {
    save("host", &format!("{host}\n"))
}

/// Stops using `host` if it's the console in use (after unpairing it). Returns whether it was.
pub fn forget_host(host: &str) -> Result<bool> {
    let path = config_dir()?.join("host");
    match std::fs::read_to_string(&path) {
        Ok(saved) if saved.trim() == host => {
            std::fs::remove_file(&path)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub fn load_token() -> Result<String> {
    if let Ok(t) = std::env::var("KICKSTAND_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let path = config_dir()?.join("token");
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("no token: run `kickstand token` or set KICKSTAND_TOKEN ({})", path.display()))?;
    Ok(token.trim().to_string())
}

/// Returns the token, generating and saving a new one first if there is none.
pub fn ensure_token() -> Result<(String, PathBuf)> {
    let path = config_dir()?.join("token");
    if let Ok(t) = std::fs::read_to_string(&path)
        && !t.trim().is_empty()
    {
        return Ok((t.trim().to_string(), path));
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, format!("{token}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((token, path))
}

/// The title that opened hbmenu last time, if one did.
pub fn host_title() -> Option<u64> {
    let text = std::fs::read_to_string(config_dir().ok()?.join("host-title")).ok()?;
    u64::from_str_radix(text.split_whitespace().next()?, 16).ok()
}

pub fn save_host_title(id: u64, name: &str) -> Result<()> {
    save("host-title", &format!("{id:016x} {name}\n"))
}

/// The desktop app's theme when it was set by hand (dark: `Some(true)`); `None` follows the system.
pub fn dark_theme() -> Option<bool> {
    match std::fs::read_to_string(config_dir().ok()?.join("theme")).ok()?.trim() {
        "dark" => Some(true),
        "light" => Some(false),
        _ => None,
    }
}

pub fn save_dark_theme(dark: Option<bool>) -> Result<()> {
    match dark {
        Some(dark) => save("theme", if dark { "dark\n" } else { "light\n" }),
        None => {
            let _ = std::fs::remove_file(config_dir()?.join("theme"));
            Ok(())
        }
    }
}
