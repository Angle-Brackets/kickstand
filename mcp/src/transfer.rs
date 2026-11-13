// SPDX-License-Identifier: GPL-3.0-only
//! How a file gets to the SD card: the fastest way available, unless the caller insists on one.
//!
//! [`Transport::Auto`] tries, in order:
//! 1. **A mounted SD card** (see [`crate::usb`]): a local file copy.
//! 2. **USB**, when a Switch is plugged into this PC and the file is at least [`USB_MIN_SIZE`]: opening
//!    Kickstand's USB interface costs a second or two, which Wi-Fi beats for small files.
//! 3. **Wi-Fi**, otherwise, and whenever USB fails (with the reason in [`Sent::note`]). After a failure, USB
//!    isn't tried again automatically for [`USB_RETRY_AFTER`], so a PC without the one-time USB setup (a udev
//!    rule on Linux, a driver on Windows) doesn't pay for the attempt on every upload.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::client::Client;
use crate::usb::{self, UsbStatus};

/// Files smaller than this go over Wi-Fi even with a cable plugged in.
pub const USB_MIN_SIZE: u64 = 4 * 1024 * 1024;
/// How long a failed automatic USB attempt keeps later automatic uploads on Wi-Fi.
pub const USB_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

/// Which way to send a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Transport {
    /// A mounted SD card, else USB for large files when a cable is in, else Wi-Fi.
    #[default]
    Auto,
    /// USB only (a mounted SD card or Kickstand's USB interface); fails rather than fall back.
    Usb,
    /// Wi-Fi only.
    Wifi,
}

/// A finished upload.
#[derive(Debug)]
pub struct Sent {
    pub size: u64,
    pub elapsed: Duration,
    /// "Wi-Fi", "USB", or the mounted SD card.
    pub via: String,
    /// Why the fastest way wasn't used, when that's worth saying (USB failed, or was skipped after a failure).
    pub note: Option<String>,
}

impl Sent {
    /// "123 bytes via USB in 1.2 s (100.00 MB/s)", plus the note.
    pub fn describe(&self) -> String {
        let secs = self.elapsed.as_secs_f64();
        let mut text = format!(
            "{} bytes via {} in {secs:.1} s ({:.2} MB/s)",
            self.size,
            self.via,
            self.size as f64 / 1e6 / secs.max(1e-6)
        );
        if let Some(note) = &self.note {
            text.push_str(&format!("; {note}"));
        }
        text
    }
}

/// The last automatic USB attempt that failed: when, and why.
static USB_FAILURE: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// What [`Transport::Auto`] does with a file of `size` bytes right now.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Usb,
    /// Wi-Fi, with a note when USB would otherwise have been used.
    Wifi(Option<String>),
}

fn plan(size: u64) -> Plan {
    match usb::status() {
        Some(UsbStatus::MountedSd(_)) => Plan::Usb,
        Some(UsbStatus::Cable { product_id }) if size >= USB_MIN_SIZE && product_id != usb::PID_HAZE_MTP => match recent_failure() {
            Some((ago, why)) => {
                Plan::Wifi(Some(format!("sent over Wi-Fi: USB failed {} s ago ({why})", ago.as_secs())))
            }
            None => Plan::Usb,
        },
        _ => Plan::Wifi(None),
    }
}

fn recent_failure() -> Option<(Duration, String)> {
    let failure = USB_FAILURE.lock().unwrap_or_else(|e| e.into_inner());
    let (at, why) = failure.as_ref()?;
    let ago = at.elapsed();
    (ago < USB_RETRY_AFTER).then(|| (ago, why.clone()))
}

fn remember_failure(why: &anyhow::Error) {
    *USB_FAILURE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), format!("{why:#}")));
}

fn forget_failure() {
    *USB_FAILURE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether [`Transport::Auto`] would send `size` bytes over USB (a cable or a mounted SD card) right now.
pub fn prefers_usb(size: u64) -> bool {
    plan(size) == Plan::Usb
}

/// Copies `local` to `remote` (already checked with [`crate::client::check_path`]), streaming from disk.
pub async fn put_file(client: &Client, local: &Path, remote: &str, transport: Transport) -> Result<Sent> {
    let size = tokio::fs::metadata(local).await.with_context(|| format!("reading {}", local.display()))?.len();
    let note = match transport {
        Transport::Usb => return over_usb(client, local, remote).await,
        Transport::Wifi => None,
        Transport::Auto => match plan(size) {
            Plan::Usb => match over_usb(client, local, remote).await {
                Ok(sent) => return Ok(sent),
                Err(e) => usb_failed(e),
            },
            Plan::Wifi(note) => note,
        },
    };
    let stats = client.upload_file(remote, local).await?;
    Ok(Sent { size: stats.size, elapsed: stats.elapsed, via: "Wi-Fi".into(), note })
}

/// [`put_file`] for bytes already in memory (a build read for other checks). Only a USB copy goes through a
/// temporary file.
pub async fn put_bytes(client: &Client, data: &[u8], remote: &str, transport: Transport) -> Result<Sent> {
    let note = match transport {
        Transport::Usb => return bytes_over_usb(client, data, remote).await,
        Transport::Wifi => None,
        Transport::Auto => match plan(data.len() as u64) {
            Plan::Usb => match bytes_over_usb(client, data, remote).await {
                Ok(sent) => return Ok(sent),
                Err(e) => usb_failed(e),
            },
            Plan::Wifi(note) => note,
        },
    };
    let stats = client.upload(remote, data).await?;
    Ok(Sent { size: stats.size, elapsed: stats.elapsed, via: "Wi-Fi".into(), note })
}

/// Records a failed automatic USB attempt and returns the note for the Wi-Fi upload that follows.
fn usb_failed(e: anyhow::Error) -> Option<String> {
    let note = format!("sent over Wi-Fi: USB failed ({e:#})");
    remember_failure(&e);
    Some(note)
}

async fn over_usb(client: &Client, local: &Path, remote: &str) -> Result<Sent> {
    let up = usb::upload(client, local, remote).await?;
    forget_failure();
    Ok(Sent { size: up.size, elapsed: up.elapsed, via: up.via, note: None })
}

async fn bytes_over_usb(client: &Client, data: &[u8], remote: &str) -> Result<Sent> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("kickstand-upload-{}-{id}", std::process::id()));
    tokio::fs::write(&tmp, data).await.with_context(|| format!("writing {}", tmp.display()))?;
    let sent = over_usb(client, &tmp, remote).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recent_usb_failure_keeps_auto_on_wifi_until_it_expires() {
        forget_failure();
        assert!(recent_failure().is_none());
        remember_failure(&anyhow::anyhow!("permission denied"));
        let (_, why) = recent_failure().expect("just failed");
        assert!(why.contains("permission denied"));
        *USB_FAILURE.lock().unwrap() = Some((Instant::now() - USB_RETRY_AFTER - Duration::from_secs(1), "old".into()));
        assert!(recent_failure().is_none());
        forget_failure();
    }

    #[test]
    fn describe_mentions_the_note() {
        let sent = Sent { size: 2_000_000, elapsed: Duration::from_secs(1), via: "Wi-Fi".into(), note: Some("sent over Wi-Fi: USB failed (x)".into()) };
        let text = sent.describe();
        assert!(text.starts_with("2000000 bytes via Wi-Fi in 1.0 s (2.00 MB/s)"), "{text}");
        assert!(text.ends_with("USB failed (x)"), "{text}");
    }
}
