// SPDX-License-Identifier: GPL-3.0-only
//! Signs a release's `SHA256SUMS` so the updater only takes releases made with Kickstand's key. The key is
//! the `KICKSTAND_SIGNING_KEY` repository secret, which the release workflow signs with; a copy is kept
//! at `~/.config/kickstand-release/signing.key` (back it up: GitHub won't show a secret again).
//!
//! - `keygen`: makes the key and prints the public key to put in `mcp/src/update.rs` (`PUBLIC_KEY`).
//! - `sign FILE`: writes `FILE.sig` (base64 of the 64-byte ed25519 signature).
//! - `public`: prints the public key again.
//!
//! `sign` and `public` take the key from `$KICKSTAND_SIGNING_KEY` when it's set, else from the file.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};

fn key_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("kickstand-release/signing.key"))
}

fn load() -> Result<SigningKey> {
    let (text, from) = match std::env::var("KICKSTAND_SIGNING_KEY").ok().filter(|k| !k.trim().is_empty()) {
        Some(key) => (key, "$KICKSTAND_SIGNING_KEY".to_string()),
        None => {
            let path = key_path()?;
            let text = std::fs::read_to_string(&path).with_context(|| format!("no signing key at {} (run `keygen` first)", path.display()))?;
            (text, path.display().to_string())
        }
    };
    let bytes: [u8; 32] = STANDARD.decode(text.trim()).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow::anyhow!("{from} isn't a key"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["keygen"] => {
            let path = key_path()?;
            if path.exists() {
                bail!("{} already exists; it signs every release, so it isn't replaced", path.display());
            }
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, format!("{}\n", STANDARD.encode(seed)))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            eprintln!("wrote {} (back it up: without it, installed copies can't take new releases)", path.display());
            println!("{}", STANDARD.encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes()));
        }
        ["public"] => println!("{}", STANDARD.encode(load()?.verifying_key().to_bytes())),
        ["sign", file] => {
            let data = std::fs::read(file).with_context(|| format!("reading {file}"))?;
            let signature = load()?.sign(&data);
            std::fs::write(format!("{file}.sig"), format!("{}\n", STANDARD.encode(signature.to_bytes())))?;
            eprintln!("wrote {file}.sig");
        }
        _ => bail!("usage: kickstand-release keygen | public | sign FILE"),
    }
    Ok(())
}
