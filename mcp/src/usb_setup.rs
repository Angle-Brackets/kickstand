// SPDX-License-Identifier: GPL-3.0-only
//! The one-time setup that lets this PC open Kickstand's USB device (`057e:3000`), the way other Switch
//! homebrew with USB (SysDVR, nxdumptool, Goldleaf) has the user do it, but from Kickstand:
//! - **Linux:** a udev rule, written as root through polkit (`pkexec`, the desktop's own password prompt),
//!   or `sudo` in a terminal without a desktop session.
//! - **Windows:** the WinUSB driver, installed with libwdi (the library Zadig is built on, through the `wdi`
//!   crate) by a second copy of this program started with Windows' own admin prompt (UAC).
//! - **macOS:** nothing; any program may open the device.
//!
//! Kickstand explains what will change in its own window first; the approval itself is always the
//! operating system's prompt, which no program can stand in for.

use anyhow::Result;
#[cfg(any(target_os = "linux", windows))]
use anyhow::{Context, bail};

/// The rule Linux needs: the logged-in user (`uaccess`) may open the device.
pub const UDEV_RULE: &str =
    r#"SUBSYSTEM=="usb", ATTRS{idVendor}=="057e", ATTRS{idProduct}=="3000", MODE="0660", TAG+="uaccess""#;
/// Where [`run`] writes [`UDEV_RULE`].
pub const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/71-kickstand.rules";

/// What [`run`] would change on this PC, for the confirmation; `None` when nothing needs doing.
pub fn needed() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        (!udev_rule_installed()).then_some(
            "Kickstand adds a rule to /etc/udev/rules.d so your user may open the Switch's Kickstand USB device. \
             Your system asks for your password once.",
        )
    }
    #[cfg(windows)]
    {
        (!driver_marker().is_some_and(|m| m.exists())).then_some(
            "Kickstand installs Windows' WinUSB driver for the Switch's Kickstand USB device, as Zadig would. \
             Windows asks for permission once, and adds a certificate for the driver to this PC's trusted list.",
        )
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

/// Does the setup, asking the OS for approval; blocks until it's done (minutes, on Windows). Returns what
/// changed. `gui` picks the desktop's prompt over a terminal's `sudo` on Linux.
pub fn run(gui: bool) -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        // A fixed script, nothing from outside in it.
        let script = format!(
            "printf '%s\\n' '{UDEV_RULE}' > {UDEV_RULE_PATH} && udevadm control --reload-rules && udevadm trigger --subsystem-match=usb"
        );
        let output = privesc::PrivilegedCommand::new("/bin/sh")
            .args(["-c", &script])
            .gui(gui)
            .prompt("Kickstand: allow your user to open the Switch's USB device")
            .run()
            .context("asking for permission")?;
        if !output.success() {
            let why = output.stderr_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            bail!("the USB setup didn't finish{}", why.map_or_else(String::new, |w| format!(": {w}")));
        }
        Ok(format!("Added {UDEV_RULE_PATH}: this PC may now use USB with the Switch"))
    }
    #[cfg(windows)]
    {
        let _ = gui;
        let exe = std::env::current_exe().context("finding this program")?;
        let output = privesc::PrivilegedCommand::new(&exe)
            .args(["usb-setup", "--install-driver"])
            .run()
            .context("asking Windows for permission")?;
        if !output.success() {
            bail!("the driver installation didn't finish (cancelled, or it failed; Kickstand's log has the details)");
        }
        if let Some(marker) = driver_marker() {
            let _ = std::fs::write(marker, "WinUSB for 057e:3000\n");
        }
        Ok("Installed WinUSB for the Switch's Kickstand USB device: this PC may now use USB with it".into())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = gui;
        Ok("Nothing to set up on this system: USB works as it is".into())
    }
}

/// Whether a udev rule for the device is installed, in any of udev's rule folders.
#[cfg(target_os = "linux")]
fn udev_rule_installed() -> bool {
    ["/etc/udev/rules.d", "/run/udev/rules.d", "/usr/lib/udev/rules.d", "/lib/udev/rules.d"].iter().any(|dir| {
        std::fs::read_dir(dir).into_iter().flatten().flatten().any(|entry| {
            std::fs::read_to_string(entry.path()).is_ok_and(|text| mentions_device(&text))
        })
    })
}

/// Whether udev rules `text` match Kickstand's device (vendor 057e, product 3000).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn mentions_device(text: &str) -> bool {
    text.lines().map(|l| l.trim()).filter(|l| !l.starts_with('#')).any(|line| {
        let line = line.to_ascii_lowercase();
        line.contains("\"057e\"") && line.contains("\"3000\"")
    })
}

/// Remembers that [`run`] installed the driver, as Windows can't be asked cheaply.
#[cfg(windows)]
fn driver_marker() -> Option<std::path::PathBuf> {
    crate::config::config_dir().ok().map(|d| d.join("usb-driver-installed"))
}

/// The elevated half on Windows (`kickstand usb-setup --install-driver`, started by [`run`]): installs
/// WinUSB for the device with libwdi, as bmputil does for the Black Magic Probe. Works without the device
/// plugged in; Windows uses the driver the next time it appears.
#[cfg(windows)]
pub fn install_driver() -> Result<()> {
    use std::ffi::CString;
    let mut device = wdi::DeviceInfo {
        vid: proto::usb::VENDOR_ID,
        pid: proto::usb::PRODUCT_ID,
        is_composite: false,
        mi: 0,
        desc: CString::new("Kickstand").unwrap().into_bytes_with_nul(),
        driver: None,
        device_id: None,
        hardware_id: Some(CString::new(r"USB\VID_057E&PID_3000").unwrap().into_bytes_with_nul()),
        compatible_id: None,
        upper_filter: None,
        driver_version: 0,
    };
    let dir = std::env::temp_dir().join("kickstand-usb-driver");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = dir.to_str().context("the temporary folder's path isn't UTF-8")?.to_string();
    wdi::prepare_driver(&mut device, &dir, "kickstand.inf", &mut Default::default())
        .map_err(|e| anyhow::anyhow!("preparing the driver: {e:?}"))?;
    wdi::install_driver(&mut device, &dir, "kickstand.inf", &mut Default::default())
        .map_err(|e| anyhow::anyhow!("installing the driver: {e:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_rules_for_the_device_only() {
        assert!(mentions_device(UDEV_RULE));
        assert!(mentions_device(
            "SUBSYSTEM==\"usb\", ATTRS{idVendor}==\"057E\", ATTRS{idProduct}==\"3000\", MODE=\"0666\""
        ));
        assert!(!mentions_device("# ATTRS{idVendor}==\"057e\", ATTRS{idProduct}==\"3000\""));
        assert!(!mentions_device("SUBSYSTEMS==\"usb\", ATTRS{manufacturer}==\"NVIDIA Corp.\", ATTRS{product}==\"APX\""));
        assert!(!mentions_device("ATTRS{idVendor}==\"057e\", ATTRS{idProduct}==\"2000\""));
    }
}
