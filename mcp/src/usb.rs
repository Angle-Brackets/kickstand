// SPDX-License-Identifier: GPL-3.0-only
//! Transferring large files (ROMs, `.o2r` archives, assets) to the Switch over USB without loading them
//! into memory.
//!
//! Two paths are supported:
//! 1. **Mounted SD card** (hekate USB Mass Storage, an SD card reader, or `$KICKSTAND_USB_MOUNT`): streams
//!    the file from disk to a staging file in `/kickstand/staging/` on the card and renames it into place.
//! 2. **Horizon OS USB MTP** (`/switch/haze.nro`, shipped with Atmosphère): when the Switch is plugged into
//!    this PC via USB (`057e:2000` or `057e:201d`), opens `/switch/haze.nro` with full memory (`Mode::Full`)
//!    if it isn't already running, waits for the desktop's MTP mount (`gvfs` / `gio`), streams the file over
//!    USB bulk endpoints with `gio copy` (~15–20 MB/s), exits `haze.nro` back to `hbmenu`, and verifies the
//!    file size on the SD card through the daemon.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use proto::Button;

use crate::client::{Client, check_path};
use crate::launch;

/// Nintendo's USB vendor ID.
const NINTENDO_VID: u16 = 0x057e;
/// Horizon OS default USB product ID when plugged into a PC.
const PID_HORIZON: u16 = 0x2000;
/// Atmosphère `haze.nro` MTP responder USB product ID.
const PID_HAZE_MTP: u16 = 0x201d;
/// Atmosphère's built-in MTP responder NRO on the SD card.
const HAZE_NRO: &str = "/switch/haze.nro";
/// Buffer size for streaming copies to a mounted SD card (1 MiB).
const COPY_BUF_SIZE: usize = 1024 * 1024;
/// How long to wait for `haze.nro` to enumerate and mount over MTP.
const MTP_MOUNT_WAIT: Duration = Duration::from_secs(12);

/// A Nintendo Switch seen on this PC's USB bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDevice {
    pub product_id: u16,
    pub product: String,
    pub serial: Option<String>,
    pub bus: Option<u8>,
    pub dev: Option<u8>,
}

/// How the Switch is reachable over USB from this PC right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbStatus {
    /// The Switch's SD card is mounted directly on the PC (hekate UMS, SD card reader, or `$KICKSTAND_USB_MOUNT`).
    MountedSd(PathBuf),
    /// Atmosphère's `haze.nro` MTP responder is already mounted (`mtp://.../SD Card`).
    MtpMounted { mount: PathBuf, uri: String },
    /// The Switch is plugged into USB (`057e:2000` or `057e:201d`).
    Attached(UsbDevice),
}

/// Result of a USB file transfer.
pub struct UsbUpload {
    pub size: u64,
    pub elapsed: Duration,
    pub via: String,
    pub notes: Vec<String>,
}

/// Checks whether the Switch is connected to this PC via USB or has its SD card mounted.
pub fn status() -> Option<UsbStatus> {
    if let Some(mount) = find_sd_mount() {
        return Some(UsbStatus::MountedSd(mount));
    }
    if let Some((mount, uri)) = find_mtp_mount() {
        return Some(UsbStatus::MtpMounted { mount, uri });
    }
    if let Some(dev) = find_usb_device() {
        return Some(UsbStatus::Attached(dev));
    }
    None
}

/// One-line summary for `status`, when the Switch is connected over USB or its SD card is mounted.
pub fn describe() -> Option<String> {
    match status()? {
        UsbStatus::MountedSd(root) => {
            Some(format!("usb: SD card mounted at {} (use usb_upload for large files)", root.display()))
        }
        UsbStatus::MtpMounted { .. } => {
            Some("usb: connected (MTP ready; use usb_upload for large files)".into())
        }
        UsbStatus::Attached(dev) => {
            let mode = if dev.product_id == PID_HAZE_MTP { ", MTP mode" } else { "" };
            let details = match dev.serial.as_deref() {
                Some(s) if !s.is_empty() => format!(" ({s}{mode})"),
                _ if !mode.is_empty() => format!(" ({})", mode.trim_start_matches(", ")),
                _ => String::new(),
            };
            Some(format!("usb: connected{details}; use usb_upload for large files"))
        }
    }
}

/// Transfers `local` to `remote` on the Switch's SD card over USB, streaming from disk without reading the
/// whole file into memory. `remote` must already be normalized and checked with [`check_path`].
pub async fn upload(client: &Client, local: &Path, remote: &str) -> Result<UsbUpload> {
    let remote = check_path(remote, true)?;
    let meta = tokio::fs::metadata(local).await.with_context(|| format!("reading {}", local.display()))?;
    anyhow::ensure!(meta.is_file(), "{} isn't a regular file", local.display());
    let size = meta.len();

    match status() {
        Some(UsbStatus::MountedSd(root)) => copy_to_mounted_sd(&root, local, &remote, size).await,
        Some(UsbStatus::MtpMounted { uri, .. }) => {
            let mut notes = Vec::new();
            prepare_sd_destination(client, &remote).await?;
            let start = Instant::now();
            gio_copy(local, &uri, &remote).await?;
            let elapsed = start.elapsed();
            verify_sd_size(client, &remote, size).await?;
            notes.push("used existing haze MTP mount".into());
            Ok(UsbUpload { size, elapsed, via: "USB MTP".into(), notes })
        }
        Some(UsbStatus::Attached(dev)) => upload_via_haze(client, local, &remote, size, dev).await,
        None => bail!(
            "no Nintendo Switch USB connection or mounted SD card was found on this PC. \
             Plug the Switch into this computer with a USB-C data cable, or use `upload` to send the file over Wi-Fi"
        ),
    }
}

/// Copies `local` directly into a mounted Switch SD card at `root`, staging in `/kickstand/staging/` on the
/// card first and renaming atomically into place.
async fn copy_to_mounted_sd(root: &Path, local: &Path, remote: &str, size: u64) -> Result<UsbUpload> {
    let root = root.to_path_buf();
    let local = local.to_path_buf();
    let remote = remote.to_string();
    tokio::task::spawn_blocking(move || {
        let rel = remote.trim_start_matches('/');
        let dest = root.join(rel);
        let parent = dest.parent().context("destination has no parent folder")?;
        let staging_dir = root.join(proto::path::STAGING_DIR.trim_start_matches('/'));
        std::fs::create_dir_all(&staging_dir).with_context(|| format!("creating {}", staging_dir.display()))?;
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

        let staging = staging_dir.join(format!("usb-{}.part", std::process::id()));
        let start = Instant::now();
        let copy_result = (|| -> Result<()> {
            let mut src = std::fs::File::open(&local).with_context(|| format!("opening {}", local.display()))?;
            let mut dst = std::fs::File::create(&staging).with_context(|| format!("creating {}", staging.display()))?;
            let mut buf = vec![0u8; COPY_BUF_SIZE];
            let mut written = 0u64;
            loop {
                let n = src.read(&mut buf).with_context(|| format!("reading {}", local.display()))?;
                if n == 0 {
                    break;
                }
                dst.write_all(&buf[..n]).with_context(|| format!("writing {}", staging.display()))?;
                written += n as u64;
            }
            anyhow::ensure!(written == size, "{} changed size during copy ({written} != {size})", local.display());
            dst.sync_data().with_context(|| format!("flushing {}", staging.display()))?;
            drop(dst);
            std::fs::rename(&staging, &dest).with_context(|| format!("moving {} to {}", staging.display(), dest.display()))?;
            Ok(())
        })();
        if copy_result.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        copy_result?;
        Ok(UsbUpload {
            size,
            elapsed: start.elapsed(),
            via: format!("SD card mount ({})", root.display()),
            notes: Vec::new(),
        })
    })
    .await?
}

/// Opens `/switch/haze.nro` with full memory (`Mode::Full`), waits for the desktop MTP mount, streams `local`
/// via `gio copy`, exits `haze.nro` back to `hbmenu`, and verifies the file size on the SD card.
async fn upload_via_haze(
    client: &Client,
    local: &Path,
    remote: &str,
    size: u64,
    dev: UsbDevice,
) -> Result<UsbUpload> {
    let gio_ok = std::process::Command::new("gio").arg("version").output().is_ok_and(|o| o.status.success());
    if !gio_ok {
        bail!(
            "the Switch is connected via USB, but `gio` (from glib2 / gvfs) isn't installed on this PC to talk to MTP; \
             install `gvfs-backends` / `glib2` or use `upload` over Wi-Fi"
        );
    }
    client
        .stat(HAZE_NRO)
        .await
        .with_context(|| format!("checking for {HAZE_NRO} on the SD card (Atmosphère's built-in USB file transfer app)"))?;

    prepare_sd_destination(client, remote).await?;

    let mut notes = Vec::new();
    let mut started_pid = None;
    if dev.product_id != PID_HAZE_MTP {
        let started = launch::start_full_tool(client, HAZE_NRO).await?;
        notes.extend(started.notes);
        started_pid = Some(started.pid);
    }

    let transfer = async {
        let (_, uri) = wait_for_mtp_mount().await?;
        let start = Instant::now();
        gio_copy(local, &uri, remote).await?;
        anyhow::Ok(start.elapsed())
    }
    .await;

    if let Some(pid) = started_pid {
        // `+` exits haze.nro cleanly back to hbmenu (still in full-memory mode), closing usb:ds so the
        // Switch re-enumerates as 057e:2000 and leaving hbmenu ready for the next launch or transfer.
        let _ = client.press(vec![Button::Plus], 1, 100).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        launch::nro_exited(pid);
        notes.push("exited haze.nro back to hbmenu (full memory)".into());
    }

    let elapsed = transfer?;
    verify_sd_size(client, remote, size).await?;
    Ok(UsbUpload { size, elapsed, via: "USB MTP (haze.nro)".into(), notes })
}

/// Ensures the parent folder of `remote` exists on the SD card and removes any existing file at `remote`
/// before `haze.nro` enumerates the folder over MTP (MTP `SendObjectInfo` refuses to overwrite an existing
/// object unless it is deleted first).
async fn prepare_sd_destination(client: &Client, remote: &str) -> Result<()> {
    match client.stat(remote).await {
        Ok(entry) if entry.is_dir => bail!("{remote} is a directory on the SD card"),
        Ok(_) => {
            client.delete(remote, false).await.with_context(|| format!("removing old {remote} before USB transfer"))?;
        }
        Err(e) if format!("{e:#}").contains("NotFound") => {
            let parent = proto::path::parent(remote);
            if client.stat(parent).await.is_err() {
                client.upload(remote, &[]).await.with_context(|| format!("creating parent folder for {remote}"))?;
                let _ = client.delete(remote, false).await;
            }
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Checks via the daemon that `remote` landed on the SD card with `expected_size` bytes.
async fn verify_sd_size(client: &Client, remote: &str, expected_size: u64) -> Result<()> {
    let entry = client.stat(remote).await.with_context(|| format!("verifying {remote} on the SD card after USB transfer"))?;
    anyhow::ensure!(
        !entry.is_dir && entry.size == expected_size,
        "USB transfer to {remote} finished, but the SD card has {} bytes instead of {expected_size}",
        entry.size
    );
    Ok(())
}

/// Streams `local` to `<mtp_root_uri><remote>` using `gio copy` (which talks directly to `gvfsd-mtp`
/// without reading the file into this process's memory).
async fn gio_copy(local: &Path, mtp_root_uri: &str, remote: &str) -> Result<()> {
    let dest_uri = format!("{mtp_root_uri}{remote}");
    let output = tokio::process::Command::new("gio")
        .arg("copy")
        .arg(local)
        .arg(&dest_uri)
        // A cancelled call (an agent cancelling the tool) stops the copy too, instead of leaving it running.
        .kill_on_drop(true)
        .output()
        .await
        .context("running `gio copy`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`gio copy` to {dest_uri} failed: {}", stderr.trim());
    }
    Ok(())
}

/// Waits up to [`MTP_MOUNT_WAIT`] for `haze.nro` to enumerate as `057e:201d` and for GVFS to mount its
/// `SD Card` storage. If auto-mount hasn't mounted it after 2 s, runs `gio mount` explicitly.
async fn wait_for_mtp_mount() -> Result<(PathBuf, String)> {
    let start = Instant::now();
    let mut manual_mount_tried = false;
    loop {
        if let Some(found) = find_mtp_mount() {
            return Ok(found);
        }
        if !manual_mount_tried
            && start.elapsed() >= Duration::from_secs(2)
            && let Some(dev) = find_usb_device()
            && dev.product_id == PID_HAZE_MTP
            && let (Some(bus), Some(devnum)) = (dev.bus, dev.dev)
        {
            manual_mount_tried = true;
            let _ = tokio::process::Command::new("gio")
                .args(["mount", &format!("mtp://[usb:{bus:03},{devnum:03}]/")])
                .output()
                .await;
        }
        if start.elapsed() > MTP_MOUNT_WAIT {
            bail!(
                "haze.nro started, but the Switch's MTP volume didn't mount within {} s (check the USB cable connects data, not just power)",
                MTP_MOUNT_WAIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Looks for a mounted Switch SD card: `$KICKSTAND_USB_MOUNT` first, then standard removable media folders.
fn find_sd_mount() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("KICKSTAND_USB_MOUNT").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(custom);
        if path.is_dir() {
            return Some(path);
        }
    }
    for base in candidate_mount_parents() {
        let Ok(entries) = std::fs::read_dir(base) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_switch_sd(&path) {
                return Some(path);
            }
        }
    }
    #[cfg(windows)]
    for letter in b'D'..=b'Z' {
        let root = PathBuf::from(format!("{}:\\", letter as char));
        if is_switch_sd(&root) {
            return Some(root);
        }
    }
    None
}

/// Whether `dir` looks like the root of a Switch SD card (has both `atmosphere/` and `switch/`).
pub(crate) fn is_switch_sd(dir: &Path) -> bool {
    dir.is_dir() && dir.join("atmosphere").is_dir() && dir.join("switch").is_dir()
}

fn candidate_mount_parents() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(user) = std::env::var_os("USER").filter(|u| !u.is_empty()) {
        dirs.push(PathBuf::from("/run/media").join(&user));
        dirs.push(PathBuf::from("/media").join(&user));
    }
    dirs.push(PathBuf::from("/Volumes"));
    dirs.push(PathBuf::from("/mnt"));
    dirs
}

/// Looks for a GVFS MTP mount of the Switch (`/run/user/<uid>/gvfs/mtp:host=Nintendo_.../SD Card`).
fn find_mtp_mount() -> Option<(PathBuf, String)> {
    let mut gvfs_dirs = Vec::new();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        gvfs_dirs.push(PathBuf::from(runtime).join("gvfs"));
    }
    if let Ok(entries) = std::fs::read_dir("/run/user") {
        for entry in entries.flatten() {
            gvfs_dirs.push(entry.path().join("gvfs"));
        }
    }
    for gvfs in gvfs_dirs {
        let Ok(entries) = std::fs::read_dir(&gvfs) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(host) = name.strip_prefix("mtp:host=")
                && host.contains("Nintendo")
            {
                let sd_card = entry.path().join("SD Card");
                if sd_card.is_dir() {
                    let uri = format!("mtp://{host}/SD Card");
                    return Some((sd_card, uri));
                }
            }
        }
    }
    None
}

/// Scans `/sys/bus/usb/devices` on Linux for a Nintendo Switch (`057e:2000` or `057e:201d`).
fn find_usb_device() -> Option<UsbDevice> {
    let entries = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        let vid = read_trimmed(&dir.join("idVendor")).and_then(|s| u16::from_str_radix(&s, 16).ok());
        if vid != Some(NINTENDO_VID) {
            continue;
        }
        let Some(pid) = read_trimmed(&dir.join("idProduct")).and_then(|s| u16::from_str_radix(&s, 16).ok()) else {
            continue;
        };
        if pid != PID_HORIZON && pid != PID_HAZE_MTP {
            continue;
        }
        let product = read_trimmed(&dir.join("product")).unwrap_or_else(|| "Nintendo Switch".into());
        let serial = read_trimmed(&dir.join("serial"));
        let bus = read_trimmed(&dir.join("busnum")).and_then(|s| s.parse().ok());
        let dev = read_trimmed(&dir.join("devnum")).and_then(|s| s.parse().ok());
        return Some(UsbDevice { product_id: pid, product, serial, bus, dev });
    }
    None
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_switch_sd_card_layout() {
        let dir = std::env::temp_dir().join(format!("kickstand-usb-sd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("switch")).unwrap();
        assert!(!is_switch_sd(&dir));
        std::fs::create_dir_all(dir.join("atmosphere")).unwrap();
        assert!(is_switch_sd(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn copies_to_mounted_sd_atomically() {
        let dir = std::env::temp_dir().join(format!("kickstand-usb-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("atmosphere")).unwrap();
        std::fs::create_dir_all(dir.join("switch")).unwrap();

        let src = dir.join("local.rom");
        let payload: Vec<u8> = (0..250_000).map(|i| (i * 17 % 253) as u8).collect();
        std::fs::write(&src, &payload).unwrap();

        let res = copy_to_mounted_sd(&dir, &src, "/switch/paperboat/paperboat.o2r", payload.len() as u64)
            .await
            .unwrap();
        assert_eq!(res.size, payload.len() as u64);
        assert_eq!(std::fs::read(dir.join("switch/paperboat/paperboat.o2r")).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
