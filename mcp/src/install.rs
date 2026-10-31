// SPDX-License-Identifier: GPL-3.0-only
//! `kickstand install`: puts the desktop app in this computer's app menu, with its icon, running this
//! copy of Kickstand. On Linux a `.desktop` entry and icons under `~/.local/share`; on macOS an app bundle
//! in `~/Applications`. Everything goes in the user's home: nothing needs root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The app icon (`assets/logo/app`): (size in px, PNG).
const ICONS: [(u32, &[u8]); 9] = [
    (16, include_bytes!("../../assets/logo/app/kickstand-16.png")),
    (24, include_bytes!("../../assets/logo/app/kickstand-24.png")),
    (32, include_bytes!("../../assets/logo/app/kickstand-32.png")),
    (48, include_bytes!("../../assets/logo/app/kickstand-48.png")),
    (64, include_bytes!("../../assets/logo/app/kickstand-64.png")),
    (128, include_bytes!("../../assets/logo/app/kickstand-128.png")),
    (256, include_bytes!("../../assets/logo/app/kickstand-256.png")),
    (512, include_bytes!("../../assets/logo/app/kickstand-512.png")),
    (1024, include_bytes!("../../assets/logo/app/kickstand-1024.png")),
];
const SVG: &[u8] = include_bytes!("../../assets/logo/app/kickstand.svg");

/// The window's icon.
pub fn window_icon() -> &'static [u8] {
    ICONS[6].1
}

/// Adds Kickstand to the app menu (`remove`: takes it out); returns what was done.
pub fn run(remove: bool) -> Result<String> {
    let exe = std::env::current_exe().context("finding this program's path")?;
    if cfg!(target_os = "macos") {
        mac(&exe, remove)
    } else if cfg!(windows) {
        bail!("adding Kickstand to the Start menu isn't supported yet: make a shortcut to {} with the argument `desktop`", exe.display())
    } else {
        linux(&exe, remove)
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")
}

fn linux(exe: &Path, remove: bool) -> Result<String> {
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home()?.join(".local/share"),
    };
    let entry = data.join("applications/kickstand.desktop");
    let icons = data.join("icons/hicolor");
    let icon_files: Vec<PathBuf> = ICONS
        .iter()
        .filter(|(size, _)| *size <= 512)
        .map(|(size, _)| icons.join(format!("{size}x{size}/apps/kickstand.png")))
        .chain(std::iter::once(icons.join("scalable/apps/kickstand.svg")))
        .collect();
    if remove {
        let mut removed = false;
        for file in std::iter::once(&entry).chain(&icon_files) {
            removed |= std::fs::remove_file(file).is_ok();
        }
        refresh(&data);
        return Ok(if removed { "removed Kickstand from the app menu".into() } else { "Kickstand wasn't in the app menu".into() });
    }
    for ((_, png), file) in ICONS.iter().filter(|(size, _)| *size <= 512).zip(&icon_files) {
        write(file, png)?;
    }
    write(icon_files.last().unwrap(), SVG)?;
    write(&entry, desktop_entry(exe).as_bytes())?;
    refresh(&data);
    let mut done = format!("added Kickstand to the app menu ({}), running {}", entry.display(), exe.display());
    if exe.components().any(|c| c.as_os_str() == "debug") {
        done.push_str("\nthat's a debug build: run `cargo build --release` and `target/release/kickstand install` for the faster one");
    }
    Ok(done)
}

/// The `.desktop` entry. `StartupWMClass` matches the window's app id, so docks show the icon.
fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Kickstand\nComment=Let an AI agent use your Switch\nExec=\"{}\" desktop\nIcon=kickstand\nTerminal=false\nCategories=Development;\nKeywords=Switch;homebrew;MCP;agent;\nStartupWMClass=kickstand\n",
        exe.display()
    )
}

/// Lets menus notice the change now; it's fine if the tool isn't installed. (No icon cache: a cache in the
/// user's folder that goes stale hides icons other apps add later.)
fn refresh(data: &Path) {
    let mut cmd = std::process::Command::new("update-desktop-database");
    cmd.arg(data.join("applications")).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().ok();
}

fn mac(exe: &Path, remove: bool) -> Result<String> {
    let app = home()?.join("Applications/Kickstand.app");
    if remove {
        if !app.exists() {
            return Ok("Kickstand wasn't in ~/Applications".into());
        }
        std::fs::remove_dir_all(&app).with_context(|| format!("removing {}", app.display()))?;
        return Ok(format!("removed {}", app.display()));
    }
    let contents = app.join("Contents");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Kickstand</string>
  <key>CFBundleDisplayName</key><string>Kickstand</string>
  <key>CFBundleIdentifier</key><string>io.github.angle-brackets.kickstand</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>kickstand</string>
  <key>CFBundleIconFile</key><string>kickstand</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"#,
        version = env!("CARGO_PKG_VERSION")
    );
    write(&contents.join("Info.plist"), plist.as_bytes())?;
    write(&contents.join("Resources/kickstand.icns"), &icns())?;
    // A launcher for this copy of Kickstand, so a rebuild is picked up without installing again.
    let launcher = contents.join("MacOS/kickstand");
    write(&launcher, format!("#!/bin/sh\nexec \"{}\" desktop\n", exe.display()).as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(format!("added {} (find it in Launchpad or Spotlight), running {}", app.display(), exe.display()))
}

/// An `.icns` file: the PNGs as they are, each under its type code.
fn icns() -> Vec<u8> {
    let types: [(&[u8; 4], u32); 6] = [(b"icp4", 16), (b"icp5", 32), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024)];
    let mut body = Vec::new();
    for (code, size) in types {
        let png = ICONS.iter().find(|(s, _)| *s == size).map(|(_, png)| *png).unwrap_or_default();
        body.extend_from_slice(code);
        body.extend_from_slice(&(8 + png.len() as u32).to_be_bytes());
        body.extend_from_slice(png);
    }
    let mut file = b"icns".to_vec();
    file.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    file.extend(body);
    file
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("no parent folder")?)?;
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn icns_holds_each_png() {
        let file = super::icns();
        assert_eq!(&file[..4], b"icns");
        assert_eq!(u32::from_be_bytes(file[4..8].try_into().unwrap()) as usize, file.len());
        // Walk the entries: each is a type, a length that includes its 8-byte header, and a PNG.
        let mut at = 8;
        let mut codes = Vec::new();
        while at < file.len() {
            let len = u32::from_be_bytes(file[at + 4..at + 8].try_into().unwrap()) as usize;
            assert_eq!(&file[at + 8..at + 12], b"\x89PNG");
            codes.push(String::from_utf8_lossy(&file[at..at + 4]).into_owned());
            at += len;
        }
        assert_eq!(codes, ["icp4", "icp5", "ic07", "ic08", "ic09", "ic10"]);
    }

    #[test]
    fn desktop_entry_runs_this_copy() {
        let entry = super::desktop_entry(std::path::Path::new("/opt/k s/kickstand"));
        assert!(entry.contains("Exec=\"/opt/k s/kickstand\" desktop\n"), "{entry}");
        assert!(entry.contains("StartupWMClass=kickstand\n"));
    }
}
