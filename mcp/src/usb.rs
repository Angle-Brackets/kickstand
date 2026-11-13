// SPDX-License-Identifier: GPL-3.0-only
//! Large files (ROMs, `.o2r` archives, assets) to the SD card over USB, streamed from disk.
//!
//! Two ways, tried in order:
//! 1. **A mounted SD card** (hekate's USB mass storage, a card reader, or `$KICKSTAND_USB_MOUNT`): a plain
//!    file copy, staged in `/kickstand/staging/` on the card and renamed into place.
//! 2. **Kickstand's own USB interface** ([`proto::usb`]): the sysmodule presents a bulk interface on the
//!    console's USB-C port on request ([`proto::Request::UsbOpen`]), and this PC talks the usual protocol
//!    over it with `nusb` (pure Rust, the same on Linux, macOS and Windows), so the upload is the Wi-Fi one
//!    (CRC-checked, staged, the same path rules) at USB speed. Nothing has to be opened on the console and
//!    whatever runs keeps running.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nusb::MaybeFuture;
use nusb::transfer::{Bulk, In, Out};
use proto::usb::{CLASS, MAX_TRANSFER, PRODUCT_ID, VENDOR_ID};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::client::{Client, check_path};

/// Horizon's own USB product id while nothing presents an interface (and haze's, while it runs).
const PID_HORIZON: u16 = 0x2000;
pub(crate) const PID_HAZE_MTP: u16 = 0x201D;
/// Buffer size for streaming copies to a mounted SD card (1 MiB).
const COPY_BUF_SIZE: usize = 1024 * 1024;
/// How long the console's interface may take to show up on this PC after [`Client::usb_open`].
const APPEAR_WAIT: Duration = Duration::from_secs(10);
/// Frames queued to the console at once: more than [`crate::client`]'s pipeline, so sending never waits.
const QUEUED_SENDS: usize = 8;

/// How the Switch is reachable over USB from this PC right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbStatus {
    /// The Switch's SD card is mounted directly on the PC (hekate UMS, a card reader, or `$KICKSTAND_USB_MOUNT`).
    MountedSd(PathBuf),
    /// A Switch is plugged into this PC (Horizon's own device, haze's, or Kickstand's interface).
    Cable { product_id: u16 },
}

/// Result of a USB file transfer.
#[derive(Debug)]
pub struct UsbUpload {
    pub size: u64,
    pub elapsed: Duration,
    pub via: String,
}

/// Whether the Switch is connected to this PC over USB or has its SD card mounted.
pub fn status() -> Option<UsbStatus> {
    if let Some(mount) = find_sd_mount() {
        return Some(UsbStatus::MountedSd(mount));
    }
    let devices = nusb::list_devices().wait().ok()?;
    devices
        .filter(|d| d.vendor_id() == VENDOR_ID && matches!(d.product_id(), PID_HORIZON | PID_HAZE_MTP | PRODUCT_ID))
        .map(|d| UsbStatus::Cable { product_id: d.product_id() })
        .next()
}

/// One line for `status`, when the Switch is connected over USB or its SD card is mounted.
pub fn describe() -> Option<String> {
    match status()? {
        UsbStatus::MountedSd(root) => Some(format!("usb: SD card mounted at {} (usb_upload copies large files there)", root.display())),
        UsbStatus::Cable { product_id: PID_HAZE_MTP } => {
            Some("usb: cable connected, but haze (MTP) has the port: exit it before usb_upload".into())
        }
        UsbStatus::Cable { .. } => Some("usb: cable connected (usb_upload sends large files over it)".into()),
    }
}

/// Copies `local` to `remote` on the SD card over USB, streaming from disk. `remote` must already be
/// normalized and checked with [`check_path`].
pub async fn upload(client: &Client, local: &Path, remote: &str) -> Result<UsbUpload> {
    let remote = check_path(remote, true)?;
    let meta = tokio::fs::metadata(local).await.with_context(|| format!("reading {}", local.display()))?;
    anyhow::ensure!(meta.is_file(), "{} isn't a regular file", local.display());
    let size = meta.len();
    if let Some(root) = find_sd_mount() {
        return copy_to_mounted_sd(&root, local, &remote, size).await;
    }
    let serial = client.usb_open().await?;
    let usb = client.over_usb(serial);
    let sent = usb.upload_file(&remote, local).await;
    // Hand the port back now rather than after the console's idle timeout.
    usb.usb_close().await;
    if sent.is_err() {
        // Over Wi-Fi too, in case the link is what broke.
        client.usb_close().await;
    }
    let stats = sent?;
    Ok(UsbUpload { size: stats.size, elapsed: stats.elapsed, via: "USB".into() })
}

/// Kickstand's interface on a USB cable: frames go out and come back as single bulk transfers.
pub struct UsbLink {
    writer: nusb::io::EndpointWrite<Bulk>,
    reader: nusb::io::EndpointRead<Bulk>,
}

impl UsbLink {
    /// Opens the console's interface with this serial number, waiting for it to appear.
    pub async fn open(serial: &str) -> Result<UsbLink> {
        let deadline = Instant::now() + APPEAR_WAIT;
        let info = loop {
            let found = nusb::list_devices()
                .await
                .context("listing USB devices")?
                .find(|d| d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID && d.serial_number() == Some(serial));
            if let Some(info) = found {
                break info;
            }
            if Instant::now() > deadline {
                bail!(
                    "the Switch's Kickstand USB device didn't appear on this PC within {} s: connect the console to this computer with a USB-C cable that carries data (straight to the console, not through the dock)",
                    APPEAR_WAIT.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let device = info.open().await.map_err(|e| match e.kind() {
            nusb::ErrorKind::PermissionDenied => anyhow!("{e}: {}", permission_hint()),
            _ => anyhow::Error::new(e).context("opening the Switch's USB device"),
        })?;
        let config = device.active_configuration().context("reading the Switch's USB configuration")?;
        let alt = config
            .interface_alt_settings()
            .find(|i| (i.class(), i.subclass(), i.protocol()) == CLASS)
            .ok_or_else(|| anyhow!("the Switch's USB device has no Kickstand interface"))?;
        let number = alt.interface_number();
        let address = |dir: nusb::transfer::Direction| {
            alt.endpoints().find(|e| e.direction() == dir).map(|e| e.address()).ok_or_else(|| anyhow!("the Kickstand interface lacks an endpoint"))
        };
        let (address_in, address_out) = (address(nusb::transfer::Direction::In)?, address(nusb::transfer::Direction::Out)?);
        let interface = device.claim_interface(number).await.map_err(|e| match e.kind() {
            nusb::ErrorKind::Busy => anyhow!("{e}: another program on this PC has the Switch's USB interface open (another kickstand copying?)"),
            nusb::ErrorKind::PermissionDenied => anyhow!("{e}: {}", permission_hint()),
            nusb::ErrorKind::Unsupported if cfg!(windows) => anyhow!("{e}: {}", permission_hint()),
            _ => anyhow::Error::new(e).context("claiming the Switch's USB interface"),
        })?;
        let writer = interface.endpoint::<Bulk, Out>(address_out)?.writer(MAX_TRANSFER).with_num_transfers(QUEUED_SENDS);
        let reader = interface.endpoint::<Bulk, In>(address_in)?.reader(MAX_TRANSFER).with_num_transfers(2);
        Ok(UsbLink { writer, reader })
    }

    /// Queues one encoded frame (prefix included) without waiting for the console to take it: it answers
    /// requests in order, and waiting here could leave it stuck handing back the previous reply.
    pub async fn send(&mut self, frame: &[u8]) -> Result<()> {
        let mut transfer = frame.to_vec();
        proto::usb::pad(&mut transfer);
        AsyncWriteExt::write_all(&mut self.writer, &transfer).await.context("sending to the Switch over USB")?;
        self.writer.submit();
        Ok(())
    }

    /// The next frame's body (without its prefix).
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let mut transfer = Vec::new();
        let mut message = self.reader.until_short_packet();
        AsyncReadExt::read_to_end(&mut message, &mut transfer).await.context("reading from the Switch over USB")?;
        message.consume_end().map_err(|e| anyhow!("{e} from the Switch"))?;
        let body = proto::usb::unpad(&transfer).ok_or_else(|| anyhow!("the Switch sent a malformed USB transfer ({} bytes)", transfer.len()))?;
        Ok(body.to_vec())
    }
}

/// Set when this PC couldn't open the device for want of the one-time setup: the MCP server then asks the
/// user whether to do it ([`take_setup_wanted`]).
static SETUP_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a USB attempt since the last call needed the one-time setup.
pub fn take_setup_wanted() -> bool {
    SETUP_WANTED.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// What to do when this PC may not open the device. Agents relay it; the setup itself is the user's to do
/// (the MCP server asks them, and starts it with the system's own approval prompt if they say so).
fn permission_hint() -> String {
    SETUP_WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let how = "the user sets this PC up once with `kickstand usb-setup` or Set up USB in Kickstand's desktop app \
               (the system asks for their approval; an agent shouldn't run it)";
    if cfg!(target_os = "linux") {
        format!(
            "this user may not open the Switch's USB device: {how}. By hand: echo '{}' | sudo tee {} && sudo udevadm control --reload-rules && sudo udevadm trigger",
            crate::usb_setup::UDEV_RULE,
            crate::usb_setup::UDEV_RULE_PATH
        )
    } else if cfg!(windows) {
        format!("Windows has no driver for the Switch's Kickstand USB device: {how}. By hand: Zadig (zadig.akeo.ie), WinUSB for \"Kickstand\"")
    } else {
        "this user may not open the Switch's USB device".into()
    }
}

/// Copies `local` directly into a mounted Switch SD card at `root`, staging in `/kickstand/staging/` on the
/// card first and renaming into place.
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

        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let staging = staging_dir.join(format!("usb-{}-{seq}.part", std::process::id()));
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
        Ok(UsbUpload { size, elapsed: start.elapsed(), via: format!("SD card mount ({})", root.display()) })
    })
    .await?
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

        let res = copy_to_mounted_sd(&dir, &src, "/switch/paperboat/paperboat.o2r", payload.len() as u64).await.unwrap();
        assert_eq!(res.size, payload.len() as u64);
        assert_eq!(std::fs::read(dir.join("switch/paperboat/paperboat.o2r")).unwrap(), payload);
        assert!(
            std::fs::read_dir(dir.join("kickstand/staging")).unwrap().next().is_none(),
            "staging file should be renamed away after completion"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_forbidden_path_or_directory_before_transfer() {
        let dir = std::env::temp_dir().join(format!("kickstand-usb-reject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rom.bin");
        std::fs::write(&file, b"data").unwrap();
        let client = Client::new("127.0.0.1:1".into(), "tok".into());

        // Forbidden remote path is rejected before touching the client or USB.
        let err = upload(&client, &file, "/atmosphere/contents/0100000000001000/exefs.nsp").await.unwrap_err();
        assert!(format!("{err:#}").contains("never written"), "{err:#}");

        // A directory as `local` is rejected with a clear message.
        let err = upload(&client, &dir, "/switch/game/rom.bin").await.unwrap_err();
        assert!(format!("{err:#}").contains("isn't a regular file"), "{err:#}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
