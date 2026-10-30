// SPDX-License-Identifier: GPL-3.0-only
//! Where the Switch's address and the shared token come from.
//!
//! - host: `--host`, then `$SWITCH_IP`, then `~/.config/kickstand/host`
//! - token: `$KICKSTAND_TOKEN`, then `~/.config/kickstand/token` (created by `kickstand token`)
//! - host title (the title Atmosphère starts hbmenu in place of, with full memory): `~/.config/kickstand/host-title`,
//!   written by `kickstand hbmenu` as `<hex id> <name>`

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("kickstand"));
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
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("host-title"), format!("{id:016x} {name}\n"))?;
    Ok(())
}
