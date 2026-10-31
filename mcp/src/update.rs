// SPDX-License-Identifier: GPL-3.0-only
//! Updates. The PC app takes the latest GitHub release built for its platform and replaces itself. The
//! Switch never goes online: the PC puts its own version's sysmodule and NRO on it over the usual
//! connection (from the PC version's release, kept in a cache), as `kickstand sysmodule install` does.
//!
//! A release counts only if its `SHA256SUMS` carries a signature by [`PUBLIC_KEY`], made on the releaser's
//! PC (`tools/release`, `scripts/publish-release.sh`), and every file downloaded must match it.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::client::Client;
use crate::launch;

const REPO: &str = "Angle-Brackets/kickstand";
/// The release key's public half (`cargo run -p kickstand-release -- public`).
const PUBLIC_KEY: &str = "Tf3beCb2ApJ5N+Q+JeR+gKvrrVU9IwIDVcco4LftK6Q=";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const NSP: &str = "sys-kickstand.nsp";
const NRO: &str = "kickstand.nro";
const SUMS: &str = "SHA256SUMS";
const MAX_DOWNLOAD: u64 = 200 << 20;
/// An agent that did something this recently is still at work, so the Switch isn't restarted under it.
const AGENT_RECENT_MS: u64 = 120_000;

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    /// Its page on GitHub, which says what changed.
    pub page: String,
    /// Each file's name and download address.
    assets: Vec<(String, String)>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(120))).user_agent(concat!("kickstand/", env!("CARGO_PKG_VERSION"))).build().into()
}

/// `releases/latest` or `releases/tags/vX`; `None` if there's no such release.
fn fetch_release(which: &str) -> Result<Option<Release>> {
    // A stand-in for GitHub's API, for trying updates out; releases still need Kickstand's signature.
    let api = std::env::var("KICKSTAND_UPDATE_API").unwrap_or_else(|_| "https://api.github.com".into());
    let url = format!("{api}/repos/{REPO}/{which}");
    let mut response = match agent().get(&url).header("Accept", "application/vnd.github+json").call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(anyhow!("asking GitHub for Kickstand's releases: {e}")),
    };
    let gh: GhRelease = response.body_mut().read_json().context("reading GitHub's answer")?;
    let version = Version::parse(gh.tag_name.trim_start_matches('v')).with_context(|| format!("release tag {} isn't a version", gh.tag_name))?;
    let assets = gh.assets.into_iter().map(|a| (a.name, a.browser_download_url)).collect();
    Ok(Some(Release { version, page: gh.html_url, assets }))
}

/// The latest release, if it's newer than this copy.
pub fn newer() -> Result<Option<Release>> {
    let current = Version::parse(VERSION)?;
    Ok(fetch_release("releases/latest")?.filter(|r| r.version > current))
}

fn download(url: &str) -> Result<Vec<u8>> {
    let mut response = agent().get(url).call().with_context(|| format!("downloading {url}"))?;
    response.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec().with_context(|| format!("downloading {url}"))
}

impl Release {
    fn url(&self, name: &str) -> Result<&str> {
        self.assets.iter().find(|(n, _)| n == name).map(|(_, url)| url.as_str()).ok_or_else(|| anyhow!("release {} has no {name}", self.version))
    }

    /// The release's checksums, once their signature checks out.
    fn sums(&self) -> Result<HashMap<String, [u8; 32]>> {
        let sums = download(self.url(SUMS)?)?;
        let signature = download(self.url(&format!("{SUMS}.sig"))?).with_context(|| format!("release {} isn't signed", self.version))?;
        verify(&sums, &signature)?;
        parse_sums(&String::from_utf8(sums).context("SHA256SUMS isn't text")?)
    }

    /// Downloads `name`, checked against `sums`.
    fn file(&self, sums: &HashMap<String, [u8; 32]>, name: &str) -> Result<Vec<u8>> {
        let expected = sums.get(name).ok_or_else(|| anyhow!("{name} isn't in release {}'s SHA256SUMS", self.version))?;
        let data = download(self.url(name)?)?;
        if Sha256::digest(&data).as_slice() != expected {
            bail!("{name} from release {} doesn't match its checksum", self.version);
        }
        Ok(data)
    }
}

fn verify(data: &[u8], signature: &[u8]) -> Result<()> {
    let key: [u8; 32] = STANDARD.decode(PUBLIC_KEY)?.try_into().map_err(|_| anyhow!("bad built-in key"))?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&key)?;
    let signature: [u8; 64] = STANDARD
        .decode(String::from_utf8_lossy(signature).trim())
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| anyhow!("the release's signature is malformed"))?;
    key.verify_strict(data, &ed25519_dalek::Signature::from_bytes(&signature)).map_err(|_| anyhow!("the release's signature doesn't match Kickstand's key: not installing it"))
}

/// `sha256sum` output: `<hex>  <name>` (or `<hex> *<name>`) per line.
fn parse_sums(text: &str) -> Result<HashMap<String, [u8; 32]>> {
    let mut sums = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (hex, name) = line.split_once(' ').ok_or_else(|| anyhow!("bad SHA256SUMS line: {line}"))?;
        let name = name.trim_start().trim_start_matches('*');
        let bytes: Vec<u8> = (0..hex.len()).step_by(2).filter_map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
        let hash: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("bad SHA256SUMS line: {line}"))?;
        sums.insert(name.to_string(), hash);
    }
    Ok(sums)
}

/// The release build for this platform, if there is one.
fn pc_target() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else {
        None
    }
}

/// Whether this copy was built here from source (it's in a cargo target directory): it updates with git.
pub fn from_source() -> bool {
    std::env::current_exe().is_ok_and(|exe| exe.ancestors().any(|dir| dir.join("CACHEDIR.TAG").exists()))
}

/// Replaces this program with `release`'s build for this platform, and keeps that release's Switch files
/// for the new copy to put on the Switch. Returns the program's path, to start it again.
pub fn update_pc(release: &Release) -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    if from_source() {
        bail!("this copy of Kickstand was built from source ({}): update it with git pull and cargo build", exe.display());
    }
    let target = pc_target().ok_or_else(|| anyhow!("there's no ready-made Kickstand for this computer yet: build it from source"))?;
    let sums = release.sums()?;
    let archive = release.file(&sums, &format!("kickstand-{}-{target}.tar.gz", release.version))?;
    let binary = extract(&archive)?;
    // Not needed for the PC; if it fails, the new copy downloads them when it updates the Switch.
    let _ = cache_switch_files(release, &sums, &release.version.to_string());
    let dir = exe.parent().ok_or_else(|| anyhow!("{} has no folder", exe.display()))?;
    let staged = dir.join(".kickstand-update");
    let write = || -> std::io::Result<()> {
        std::fs::write(&staged, &binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        // Atomic, and the running program keeps its old file open meanwhile.
        std::fs::rename(&staged, &exe)
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&staged);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            bail!("can't replace {}: this user can't write to {}. Download the new version from https://github.com/{REPO}/releases instead", exe.display(), dir.display());
        }
        return Err(anyhow!(e).context(format!("replacing {}", exe.display())));
    }
    Ok(exe)
}

/// The `kickstand` program inside a release's `.tar.gz`.
fn extract(archive: &[u8]) -> Result<Vec<u8>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() && entry.path()?.file_name().is_some_and(|n| n == "kickstand") {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            return Ok(data);
        }
    }
    bail!("the release's archive has no kickstand program in it")
}

fn cache_dir(version: &str) -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".cache"),
    };
    Ok(base.join("kickstand/switch").join(version))
}

pub struct SwitchFiles {
    pub nsp: Vec<u8>,
    pub nro: Vec<u8>,
}

fn cache_switch_files(release: &Release, sums: &HashMap<String, [u8; 32]>, version: &str) -> Result<SwitchFiles> {
    let files = SwitchFiles { nsp: release.file(sums, NSP)?, nro: release.file(sums, NRO)? };
    let dir = cache_dir(version)?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(NSP), &files.nsp)?;
    std::fs::write(dir.join(NRO), &files.nro)?;
    Ok(files)
}

/// This PC's version of the Switch's sysmodule and NRO: from the cache, else from this version's release.
pub fn switch_files() -> Result<SwitchFiles> {
    let dir = cache_dir(VERSION)?;
    if let (Ok(nsp), Ok(nro)) = (std::fs::read(dir.join(NSP)), std::fs::read(dir.join(NRO))) {
        return Ok(SwitchFiles { nsp, nro });
    }
    let release = fetch_release(&format!("releases/tags/v{VERSION}"))?.ok_or_else(|| {
        anyhow!("there's no release of Kickstand {VERSION} to update the Switch from; a copy built from source installs its own build with `kickstand sysmodule install`")
    })?;
    let sums = release.sums()?;
    cache_switch_files(&release, &sums, VERSION)
}

/// Whether the Switch's Kickstand (`version`, as it reports itself) is older than this PC's.
pub fn switch_behind(version: &str) -> bool {
    match (Version::parse(version), Version::parse(VERSION)) {
        (Ok(switch), Ok(pc)) => switch < pc,
        _ => false,
    }
}

/// Why the Switch can't be updated right now, if it can't: restarting its service would cut off whatever
/// is using it.
pub async fn switch_busy(client: &Client) -> Result<Option<String>> {
    if client.hello().await?.kind == proto::DaemonKind::Nro {
        return Ok(Some("Kickstand is open on the Switch's screen: press + on the Switch to close it first".into()));
    }
    if client.status().await?.application.is_some() {
        return Ok(Some("software is running on the Switch: close it first".into()));
    }
    if let Some(report) = client.agents().await?
        && report.agents.iter().any(|a| !a.you && a.app.is_some() && a.idle_ms < AGENT_RECENT_MS)
    {
        return Ok(Some("an agent is using the Switch: try again once it's done".into()));
    }
    Ok(None)
}

/// Puts this PC's version of Kickstand on the Switch: the sysmodule, restarted (about 15 s, no reboot), and
/// the NRO. Returns what happened, line by line.
pub async fn update_switch(client: &Client) -> Result<Vec<String>> {
    if let Some(why) = switch_busy(client).await? {
        bail!("{why}");
    }
    let files = tokio::task::spawn_blocking(switch_files).await??;
    client.install_sysmodule(&files.nsp).await?;
    launch::restart_sysmodule(client, Some(files.nro)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    #[test]
    fn reads_sha256sums() {
        let hex = "ab".repeat(32);
        let sums = parse_sums(&format!("{hex}  kickstand.nro\n{hex} *sys-kickstand.nsp\n")).unwrap();
        assert_eq!(sums["kickstand.nro"], [0xab; 32]);
        assert_eq!(sums["sys-kickstand.nsp"], [0xab; 32]);
        assert!(parse_sums("abc  x\n").is_err());
    }

    #[test]
    fn refuses_a_signature_from_another_key() {
        let other = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let signature = STANDARD.encode(other.sign(b"sums").to_bytes());
        let err = verify(b"sums", signature.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("doesn't match"), "{err}");
        assert!(verify(b"sums", b"not base64").is_err());
    }

    #[test]
    fn compares_versions() {
        assert!(switch_behind("0.0.1"));
        assert!(!switch_behind(VERSION));
        assert!(!switch_behind("99.0.0"));
        assert!(!switch_behind("mock"));
    }
}
