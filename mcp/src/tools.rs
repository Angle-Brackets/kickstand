// SPDX-License-Identifier: GPL-3.0-only
//! The MCP tools. Each one is a thin wrapper over [`Client`]; failures come back as tool errors
//! (`isError: true`) with the daemon's message, so the agent can read and react to them.

use std::fmt::Write as _;
use std::sync::Arc;

use base64::Engine;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::client::{Client, check_path};

/// Default and maximum bytes `read_file` returns inline; use `download` for more.
const READ_DEFAULT: u64 = 64 * 1024;
const READ_MAX: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct Tools {
    client: Arc<Client>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PathArg {
    /// Absolute path from the SD card root, e.g. /switch/paperboat or /atmosphere/crash_reports.
    pub path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadArgs {
    /// Absolute path from the SD card root.
    pub path: String,
    /// Byte offset to start at (default 0).
    pub offset: Option<u64>,
    /// Bytes to read (default 65536, max 1048576).
    pub length: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DownloadArgs {
    /// File on the SD card, absolute from its root.
    pub path: String,
    /// Where to save it on this PC.
    pub local_path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UploadArgs {
    /// File on this PC to send.
    pub local_path: String,
    /// Destination on the SD card. Must be under /switch/paperboat/, /switch/ttydrecomp/, /switch/kickstand/, /kickstand/ or the sysmodule's /atmosphere/contents/42000000004D4350/.
    pub remote_path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DeleteArgs {
    /// File or directory on the SD card. Must be under /switch/paperboat/, /switch/ttydrecomp/, /switch/kickstand/, /kickstand/ or the sysmodule's /atmosphere/contents/42000000004D4350/.
    pub path: String,
    /// Delete a directory and everything in it (default false: only files and empty directories).
    pub recursive: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SysmoduleAction {
    /// Whether it's installed, starts at boot, and is running.
    Status,
    /// Upload a built sys-kickstand.nsp (local_path) and the token. Doesn't enable start-at-boot.
    Install,
    /// Start it (it keeps running until reboot or stop).
    Start,
    /// Stop it (only from the NRO daemon).
    Stop,
    /// Start it, end the NRO daemon, and wait until the sysmodule answers.
    Handover,
    /// Start it at every boot (creates boot2.flag; only with the user's go-ahead).
    Enable,
    /// Stop starting it at boot (deletes boot2.flag).
    Disable,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SysmoduleArgs {
    pub action: SysmoduleAction,
    /// For install: the sys-kickstand.nsp to upload.
    pub local_path: Option<String>,
}

pub fn describe_target(target: proto::HekateTarget, entries: &[String]) -> String {
    match target {
        proto::HekateTarget::Menu => "hekate menu".into(),
        proto::HekateTarget::Ums => "USB mass storage of the SD card".into(),
        proto::HekateTarget::Entry(i) => {
            format!("entry {i} ({})", entries.get(i as usize - 1).map_or("?", String::as_str))
        }
    }
}

fn describe(s: &proto::SysmoduleStatus) -> String {
    let running = s.pid.map_or("not running".to_string(), |pid| format!("running (pid {pid})"));
    let boot = if s.boot2 { "starts at boot" } else { "does not start at boot" };
    let installed = if s.installed { "installed" } else { "not installed" };
    format!("sysmodule: {installed}, {boot}, {running}")
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RebootArgs {
    /// "menu" (hekate's menu; someone must tap an entry), "ums" (USB mass storage of the SD card), or a
    /// hekate_ipl.ini entry by name or number (from 1), e.g. the Atmosphère entry to come straight back.
    pub target: String,
    /// Must be true, and only after the user agreed to this reboot.
    pub confirm: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PressArgs {
    /// Buttons pressed together, e.g. "a", "home", "zl+zr". Names: a b x y l r zl zr plus minus up down
    /// left right lstick rstick home capture.
    pub buttons: String,
    /// How many times to press them (1-10, default 1). The lock screen wants the same button 3 times.
    pub times: Option<u8>,
    /// How long each press is held, in milliseconds (1-2000, default 100).
    pub hold_ms: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct KeepAwakeArgs {
    /// Keep the screen on for this many minutes from now (max 480); 0 ends it now; leave out to just ask.
    pub minutes: Option<u32>,
}

fn ok(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn fail(e: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))])
}

#[tool_router]
impl Tools {
    pub fn new(client: Arc<Client>) -> Self {
        Tools { client, tool_router: Self::tool_router() }
    }

    #[tool(description = "Check the daemon on the Switch is reachable. Reports round-trip time, console uptime, daemon version and whether it runs as the sysmodule or the test NRO.")]
    async fn ping(&self) -> CallToolResult {
        let result = async {
            let (version, kind) = self.client.hello().await?;
            let (rtt, uptime) = self.client.ping().await?;
            anyhow::Ok(format!(
                "daemon {version} ({kind:?}) at {}: rtt {:.1} ms, console uptime {}s",
                self.client.addr(),
                rtt.as_secs_f64() * 1000.0,
                uptime.as_secs()
            ))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Resource limit usage of the daemon's group: memory, threads, events, transfer memories, sessions (current, peak, limit), plus the daemon's own memory. From the sysmodule this is the limit all system processes share, so it shows how close the system is to running out.")]
    async fn resources(&self) -> CallToolResult {
        let result = async { anyhow::Ok(crate::client::describe_resources(&self.client.resources().await?)) };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Press buttons on the Switch through a virtual Pro Controller that the daemon attaches for the call and detaches after (about a second to settle, then the presses). Use it to get past the lock screen (b three times; after an agent reboot to an entry the sysmodule does this itself; b does nothing on the home menu, unlike a, which would launch the selected title) or to drive menus and games. You can't see the screen yet, so say what you pressed.")]
    async fn press(&self, Parameters(args): Parameters<PressArgs>) -> CallToolResult {
        let result = async {
            let buttons = crate::client::parse_buttons(&args.buttons)?;
            let times = args.times.unwrap_or(1);
            self.client.press(buttons, times, args.hold_ms.unwrap_or(100)).await?;
            anyhow::Ok(format!("pressed {} x{times}", args.buttons))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Keep the Switch's screen on (no dimming or auto-sleep) for a stretch with no other tool calls, e.g. while watching a game run. Every tool call already keeps it on for 5 minutes, after which the console's own timers apply again to save battery; call with minutes: 0 when done with a long stretch. Not kept across reboots; after a reboot to an entry, the sysmodule gets past the lock screen itself. Changes no system settings. It can't wake a console that is already asleep.")]
    async fn keep_awake(&self, Parameters(args): Parameters<KeepAwakeArgs>) -> CallToolResult {
        let result = async {
            let s = self.client.keep_awake(args.minutes).await?;
            anyhow::Ok(if s == 0 { "keep awake is off".to_string() } else { format!("screen stays on for {}m {}s", s / 60, s % 60) })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "List a directory on the Switch's SD card. Keys, NAND backups and /Nintendo are not readable.")]
    async fn list_dir(&self, Parameters(args): Parameters<PathArg>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let (mut entries, truncated) = self.client.list_dir(&path).await?;
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
            let mut out = format!("{path} ({} entries)\n", entries.len());
            for e in &entries {
                if e.is_dir {
                    let _ = writeln!(out, "  {}/", e.name);
                } else {
                    let _ = writeln!(out, "  {}  {} bytes", e.name, e.size);
                }
            }
            if truncated {
                let _ = writeln!(out, "  ... truncated at {} entries", proto::MAX_DIR_ENTRIES);
            }
            anyhow::Ok(out)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Size and type of a file or directory on the Switch's SD card.")]
    async fn stat(&self, Parameters(args): Parameters<PathArg>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let e = self.client.stat(&path).await?;
            anyhow::Ok(if e.is_dir { format!("{path}: directory") } else { format!("{path}: file, {} bytes", e.size) })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Read part of a file on the Switch's SD card, such as a log or a crash report in /atmosphere/crash_reports. Text comes back as text; binary data as base64.")]
    async fn read_file(&self, Parameters(args): Parameters<ReadArgs>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let offset = args.offset.unwrap_or(0);
            let len = args.length.unwrap_or(READ_DEFAULT).min(READ_MAX);
            let (data, size) = self.client.read(&path, offset, len).await?;
            let end = offset + data.len() as u64;
            let header = format!("{path}: bytes {offset}..{end} of {size}");
            anyhow::Ok(match String::from_utf8(data) {
                Ok(text) => format!("{header}\n{text}"),
                Err(e) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(e.into_bytes());
                    format!("{header} (binary, base64)\n{b64}")
                }
            })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Copy a whole file from the Switch's SD card to this PC.")]
    async fn download(&self, Parameters(args): Parameters<DownloadArgs>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let size = self.client.stat(&path).await?.size;
            let (data, _) = self.client.read(&path, 0, size).await?;
            tokio::fs::write(&args.local_path, &data).await?;
            anyhow::Ok(format!("saved {path} ({} bytes) to {}", data.len(), args.local_path))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Upload a file from this PC to the Switch's SD card, e.g. a fresh .nro build. Verified with CRC-32 before it replaces the old file. Only /switch/paperboat/, /switch/ttydrecomp/, /switch/kickstand/, /kickstand/ and the sysmodule's /atmosphere/contents/42000000004D4350/ are writable.")]
    async fn upload(&self, Parameters(args): Parameters<UploadArgs>) -> CallToolResult {
        let result = async {
            let remote = check_path(&args.remote_path, true)?;
            let data = tokio::fs::read(&args.local_path).await?;
            let stats = self.client.upload(&remote, &data).await?;
            let secs = stats.elapsed.as_secs_f64();
            anyhow::Ok(format!(
                "uploaded {} -> {remote}: {} bytes in {secs:.1}s ({:.2} MB/s)",
                args.local_path,
                stats.size,
                stats.size as f64 / 1e6 / secs.max(1e-6)
            ))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Manage the kickstand sysmodule, the always-on daemon: status, install (upload a built .nsp), start, stop, or handover (start it and end the hbmenu NRO daemon). Never enables start-at-boot; that needs the user's go-ahead.")]
    async fn sysmodule(&self, Parameters(args): Parameters<SysmoduleArgs>) -> CallToolResult {
        let result = async {
            let status = match args.action {
                SysmoduleAction::Status => self.client.sysmodule(proto::Request::SysmoduleStatus).await?,
                SysmoduleAction::Start => self.client.sysmodule(proto::Request::SysmoduleStart).await?,
                SysmoduleAction::Stop => self.client.sysmodule(proto::Request::SysmoduleStop).await?,
                SysmoduleAction::Handover => self.client.handover().await?,
                SysmoduleAction::Enable => self.client.set_boot2(true).await?,
                SysmoduleAction::Disable => self.client.set_boot2(false).await?,
                SysmoduleAction::Install => {
                    let local = args.local_path.ok_or_else(|| anyhow::anyhow!("install needs local_path"))?;
                    let nsp = tokio::fs::read(&local).await?;
                    self.client.install_sysmodule(&nsp).await?;
                    self.client.sysmodule(proto::Request::SysmoduleStatus).await?
                }
            };
            anyhow::Ok(describe(&status))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "List hekate's boot entries (hekate_ipl.ini), numbered as reboot expects.")]
    async fn hekate_entries(&self) -> CallToolResult {
        let result = async {
            let entries = self.client.hekate_entries().await?;
            let mut out = String::from("hekate entries:\n");
            for (i, e) in entries.iter().enumerate() {
                let _ = writeln!(out, "  {}. {e}", i + 1);
            }
            anyhow::Ok(out)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Warm-reboot the Switch into hekate (Atmosphère reboot-to-payload; never a cold reboot, which would lose an RCM-injected hekate). target picks what hekate does: an entry (boots straight back, e.g. into Atmosphère), \"ums\" (SD card over USB) or \"menu\". Only with the user's go-ahead (confirm: true). The daemon comes back only if the sysmodule starts at boot.")]
    async fn reboot(&self, Parameters(args): Parameters<RebootArgs>) -> CallToolResult {
        let result = async {
            if !args.confirm {
                anyhow::bail!("not rebooting: set confirm to true once the user has agreed");
            }
            let entries = self.client.hekate_entries().await?;
            let target = crate::client::parse_hekate_target(&args.target, &entries)?;
            self.client.reboot_to_hekate(target).await?;
            anyhow::Ok(format!("rebooting into hekate: {}", describe_target(target, &entries)))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Delete a file or directory on the Switch's SD card, e.g. old builds or test output. Only inside the writable folders (see upload); directories must be empty unless recursive is true.")]
    async fn delete(&self, Parameters(args): Parameters<DeleteArgs>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, true)?;
            let was_dir = self.client.delete(&path, args.recursive.unwrap_or(false)).await?;
            anyhow::Ok(format!("deleted {path}{}", if was_dir { " (directory)" } else { "" }))
        };
        result.await.map_or_else(fail, ok)
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "kickstand",
    instructions = "Tools for a real Nintendo Switch running the kickstand daemon. Paths are absolute from the SD card root. Start with ping."
)]
impl ServerHandler for Tools {}
