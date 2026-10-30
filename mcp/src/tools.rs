// SPDX-License-Identifier: GPL-3.0-only
//! The MCP tools. Each one is a thin wrapper over [`Client`]; failures come back as tool errors
//! (`isError: true`) with the daemon's message, so the agent can read and react to them.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::Context as _;
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
    /// The stdout of the last NRO run_nro started.
    output: Arc<std::sync::Mutex<crate::launch::OutputLog>>,
    collector: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// The ELF (and module name) run_nro or a memory tool was last given, for the memory tools' symbols.
    elf: Arc<std::sync::Mutex<Option<ElfChoice>>>,
    tool_router: ToolRouter<Self>,
}

/// An ELF path and the module name its addresses use.
type ElfChoice = (String, Option<String>);

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
    /// For install: a newer Kickstand NRO (kickstand.nro) to send on the way.
    pub nro_path: Option<String>,
    /// For install: restart the sysmodule onto the new build now (default true, about 20 s, through the
    /// Kickstand NRO in applet mode); false only uploads it.
    pub restart: Option<bool>,
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
pub struct InputArgs {
    /// Space-separated steps, each WHAT:MS, held for MS milliseconds. WHAT joins with "+": button names,
    /// "l=X,Y" / "r=X,Y" for the left/right stick (-1.0..1.0, y up; centred if absent), "touch=X,Y" for a
    /// finger on the 1280x720 touch screen, or "-" for nothing (a pause). Held things stay held across
    /// consecutive steps. Examples: "r:300 r+a:150 r:6000" (R through a launch), "l=0,1:1500" (walk up
    /// 1.5 s), "l=1,0+b:800" (run right), "touch=640,360:100" (tap the centre). At most 64 steps, 20 s.
    pub steps: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RunNroArgs {
    /// An NRO on this PC to send and run (e.g. a fresh build).
    pub local_path: Option<String>,
    /// Where to put local_path on the SD card (default /switch/kickstand/<file name>). Must be under
    /// /switch/ inside the write roots, in a directory that exists.
    pub dest: Option<String>,
    /// Instead of local_path: an NRO already on the SD card, e.g. /switch/paperboat/paperboat.nro.
    pub sd_path: Option<String>,
    /// Arguments for the NRO (argv[1..]).
    pub args: Option<Vec<String>>,
    /// Seconds to collect output before returning, less if it exits (default 5, max 60); nro_output
    /// reads the rest.
    pub wait_seconds: Option<u32>,
    /// The NRO's ELF on this PC (with debug info): backtrace addresses in its output, written
    /// `<module>+0x<offset>`, get their function and source line.
    pub elf: Option<String>,
    /// The module name its backtraces use (default: the ELF's file name without .elf).
    pub module: Option<String>,
    /// "full" (default: in a game's place with the console's full memory; games need it) or "applet" (hbmenu
    /// in the Album, a few hundred MB: small tools only, refused for games).
    pub mode: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TestBuildArgs {
    /// A project from ~/.config/kickstand/projects.json (e.g. "paperboat").
    pub project: String,
    /// Run the files as they are, without building (default false).
    pub skip_build: Option<bool>,
    /// Arguments for the NRO (default: the project's).
    pub args: Option<Vec<String>>,
    /// Watch it for this many seconds if nothing happens (default 60, at most 300).
    pub seconds: Option<u64>,
    /// Seconds without output before the screen is checked for a hang (default 20).
    pub quiet_seconds: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SuperviseArgs {
    /// Stop watching after this many seconds if nothing happens (default 60, at most 300).
    pub seconds: Option<u64>,
    /// Seconds without output before the screen is checked for a hang (default 20).
    pub quiet_seconds: Option<u64>,
    /// The NRO's ELF, for symbolized crash reports and backtraces (default: the last ELF given).
    pub elf: Option<String>,
    /// On a fatal error, reboot into hekate entry 1 right away (default true; the console can't recover
    /// any other way).
    pub reboot_on_fatal: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct StatusArgs {
    /// Include a small screenshot (default true).
    pub screenshot: Option<bool>,
    /// Its width in pixels (default 480; the screen is 1280).
    pub width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct OpenHbmenuArgs {
    /// "applet" (default) or "full"; see the tool's description.
    pub mode: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CloseAppArgs {
    /// "applet" (hbmenu or an NRO in the Album) or "application" (a game, or hbmenu or an NRO with full
    /// memory). Default: the applet if hbloader runs there, otherwise the application.
    pub target: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SymbolizeArgs {
    /// The module's ELF on this PC (with debug info).
    pub elf: String,
    /// Text containing `<module>+0x<offset>` addresses (a log, a backtrace), or bare hex offsets one per
    /// line.
    pub text: String,
    /// The module name used in the text (default: the ELF's file name without .elf).
    pub module: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CrashReportsArgs {
    /// How many to list, newest first (default 5).
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CrashReportArgs {
    /// A path or file name from crash_reports (e.g. 02637676358_01006fe013472000.log or
    /// report_52b130e42.bin).
    pub name: String,
    /// The crashed module's ELF on this PC, to symbolize `<module> + 0x...` addresses.
    pub elf: Option<String>,
    /// The module name the report uses (default: the ELF's file name without .elf).
    pub module: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NroOutputArgs {
    /// First let it run this many seconds (default 0, max 60), less if it exits.
    pub wait_seconds: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LaunchTitleArgs {
    /// The title's id (16 hex digits) or part of its name, as list_apps shows them.
    pub title: String,
    /// Which user account to pick if the title asks who is playing (0 = the first, default).
    pub user: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListAppsArgs {
    /// How many to list (default 12, the home menu's row; at most 64).
    pub max: Option<u16>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct KeepAwakeArgs {
    /// Keep the console awake for this many minutes from now (max 480; default 30, the same as any tool
    /// call); 0 lets it sleep now.
    pub minutes: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ScreenshotArgs {
    /// Also save the JPEG (full size) to this path on this PC.
    pub local_path: Option<String>,
    /// Scale the image you get to this many pixels wide (e.g. 640 costs about a quarter of the tokens of
    /// the full 1280); enough to see what's on screen, not to read small text.
    pub width: Option<u32>,
    /// Only this part of the 1280x720 screen, "x,y,w,h" (e.g. a dialog or a status bar, to read it at full
    /// resolution cheaply).
    pub region: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WaitScreenArgs {
    /// "change" (default): until the screen differs from now, e.g. a load ends or a dialog appears. "still":
    /// until it stops changing, e.g. a transition or scrolling text finishes.
    pub until: Option<String>,
    /// Give up after this many seconds (default 10, max 60) and return the screen as it is.
    pub timeout_seconds: Option<f64>,
    /// Only watch this part of the 1280x720 screen, "x,y,w,h" (e.g. a text box), ignoring the rest.
    pub region: Option<String>,
    /// Width of the image you get back (default 640; 1280 for full size).
    pub width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordGifArgs {
    /// Where to save the GIF on this PC (default: ~/Pictures/kickstand/gif-<unix time>.gif).
    pub local_path: Option<String>,
    /// How long to record (default 3, max 30).
    pub seconds: Option<f64>,
    /// Target frames per second (default 5, max 10); the real rate is lower if capturing can't keep up.
    pub fps: Option<u32>,
    /// GIF width in pixels (default 640; the console's screen is 1280).
    pub width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MemoryMapArgs {
    /// The NRO's ELF on this PC, to name the symbol each region starts at (default: the last ELF given to
    /// run_nro or a memory tool).
    pub elf: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadMemoryArgs {
    /// An address expression: 0x… (absolute), nro+0x… (in the homebrew's NRO; also its module name, e.g.
    /// Paperboat+0x…), heap+0x…, a symbol from the ELF (e.g. gPlayerStatus), any of those plus or minus
    /// offsets, and [expr] for the 64-bit pointer stored at expr (e.g. [gCurrentEncounter]+0x10).
    pub address: String,
    /// Bytes to read (default: one value of the format, 64 for hex, 256 for str; at most 1048576).
    pub length: Option<usize>,
    /// How to show it: hex (a hexdump, default), u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, ptr (64-bit
    /// pointers, named when they point into the NRO) or str (up to a NUL).
    pub format: Option<String>,
    /// The NRO's ELF on this PC, for symbol names (default: the last ELF given to run_nro or a memory tool).
    pub elf: Option<String>,
    /// The module name for `<module>+0x…` (default: the ELF's file name without .elf).
    pub module: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteMemoryArgs {
    /// An address expression, as read_memory takes.
    pub address: String,
    /// The value: a number for the integer and float formats (0x… allowed), text for str (written with a
    /// NUL after it), hex digits for hex (raw bytes, spaces allowed).
    pub value: String,
    /// u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, ptr, str or hex.
    pub format: String,
    pub elf: Option<String>,
    pub module: Option<String>,
}

/// `~/Pictures/kickstand/<ext>-<unix time>.<ext>`, creating the folder: where captures go when the agent
/// doesn't say, so the user knows where to look.
fn default_capture_path(ext: &str) -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME isn't set; pass local_path"))?;
    let dir = std::path::Path::new(&home).join("Pictures/kickstand");
    std::fs::create_dir_all(&dir)?;
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    Ok(dir.join(format!("{ext}-{secs}.{ext}")))
}

/// What `keep_awake` renews with no arguments: the daemon's per-request lease.
const DEFAULT_KEEP_AWAKE_MINUTES: u32 = 30;

impl Tools {
    /// The ELF to use: `elf` (remembered for next time) or the last one given.
    fn elf(&self, elf: Option<String>, module: Option<String>) -> anyhow::Result<crate::memory::Elf> {
        let mut last = self.elf.lock().unwrap();
        if let Some(elf) = elf {
            *last = Some((elf, module.clone()));
        }
        let (elf, remembered_module) = last.clone().unzip();
        crate::memory::Elf::load(elf.as_deref().map(std::path::Path::new), module.or(remembered_module.flatten()).as_deref())
    }

    /// Waits up to `wait_s` for the NRO to exit, then returns its new output and status.
    async fn read_output(&self, wait_s: u32) -> String {
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(u64::from(wait_s));
        while self.output.lock().unwrap().is_open() && tokio::time::Instant::now() < until {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        let (new, open) = self.output.lock().unwrap().take_new();
        let status = if open { "the NRO is running" } else { "the NRO's stdout is closed (it exited)" };
        if new.is_empty() { format!("\n[no new output; {status}]") } else { format!("\n--- output ---\n{new}\n[{status}]") }
    }
}

/// "x,y,w,h".
fn parse_region(r: &str) -> anyhow::Result<(u32, u32, u32, u32)> {
    let n: Vec<u32> = r.split(',').map(|v| v.trim().parse()).collect::<Result<_, _>>().context("region is x,y,w,h")?;
    let [x, y, w, h] = n[..] else { anyhow::bail!("region is x,y,w,h") };
    Ok((x, y, w, h))
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
        Tools { client, output: Default::default(), collector: Default::default(), elf: Default::default(), tool_router: Self::tool_router() }
    }

    #[tool(description = "Check the daemon on the Switch is reachable. Reports round-trip time, console uptime, daemon version and whether it runs as the sysmodule or the test NRO.")]
    async fn ping(&self) -> CallToolResult {
        let result = async {
            let info = self.client.hello().await?;
            let (rtt, uptime) = self.client.ping().await?;
            anyhow::Ok(format!(
                "daemon {} ({:?}, protocol {}) at {}: rtt {:.1} ms, console uptime {}s",
                info.version,
                info.kind,
                info.protocol,
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

    #[tool(description = "Press buttons on the Switch through a virtual Pro Controller that the daemon attaches for the call and detaches after (about a second to settle, then the presses). Use it to get past the lock screen (b three times; after an agent reboot to an entry the sysmodule does this itself; b does nothing on the home menu, unlike a, which would launch the selected title) or to drive menus and games. Check the result with screenshot.")]
    async fn press(&self, Parameters(args): Parameters<PressArgs>) -> CallToolResult {
        let result = async {
            let buttons = crate::client::parse_buttons(&args.buttons)?;
            let times = args.times.unwrap_or(1);
            self.client.press(buttons, times, args.hold_ms.unwrap_or(100)).await?;
            anyhow::Ok(format!("pressed {} x{times}", args.buttons))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Play timed input on one virtual controller, for what press can't do: analog sticks (moving in games), touch screen taps and drags, holding a button through other presses or a load, long holds, quick combos. Each step holds exactly what it lists for its time; everything is released at the end. Look with screenshot afterwards.")]
    async fn input(&self, Parameters(args): Parameters<InputArgs>) -> CallToolResult {
        let result = async {
            let steps = crate::client::parse_steps(&args.steps)?;
            let n = steps.len();
            self.client.input(steps).await?;
            anyhow::Ok(format!("played {n} steps"))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Where the console is, in one call: what runs (the application: a game, or hbloader with hbmenu or an NRO with full memory; and applet mode: hbloader in the Album), whether the console is stuck on a fatal error screen (then reboot it), whether the sysmodule is still pressing B to get past the lock screen after a reboot, the keep-awake lease, uptime, and a small screenshot of the screen (screenshot: false to skip it). Check it before pressing buttons, after a reboot, and whenever unsure what's on screen.")]
    async fn status(&self, Parameters(args): Parameters<StatusArgs>) -> CallToolResult {
        let result = async {
            let status = self.client.status().await?;
            let fatal = self.client.fatal().await?;
            let mut text = crate::client::describe_status(&status);
            if let Some(fatal) = &fatal {
                text = format!("{fatal}\n{text}");
            }
            let mut content = vec![ContentBlock::text(text)];
            if args.screenshot.unwrap_or(true) && fatal.is_none() {
                let jpeg = self.client.screenshot().await?;
                let width = args.width.unwrap_or(480);
                let jpeg = tokio::task::spawn_blocking(move || crate::capture::reframe(&jpeg, None, Some(width))).await??;
                content.push(ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(&jpeg), "image/jpeg"));
            }
            anyhow::Ok(content)
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Close homebrew or a game. By default the hbmenu applet (hbmenu or an NRO in applet mode) if it's open, otherwise the application (hbmenu or an NRO with full memory, or a game); target picks one. The applet is closed with HOME (it's never terminated: that crashes the home menu); the application (a game, or an NRO with full memory) is terminated immediately and can't save first, so only close a game the user isn't playing.")]
    async fn close_app(&self, Parameters(args): Parameters<CloseAppArgs>) -> CallToolResult {
        let result = async {
            let applet = match args.target.as_deref() {
                Some("applet") => true,
                Some("application") => false,
                Some(other) => anyhow::bail!("target is \"applet\" or \"application\", not {other:?}"),
                None => self.client.applet_status().await?.is_some_and(|a| a.hbl == Some(true)),
            };
            let mode = if applet { crate::launch::Mode::Applet } else { crate::launch::Mode::Full };
            anyhow::Ok(match crate::launch::close_running(&self.client, mode).await? {
                Some((app, how)) => format!("closed {} ({})", crate::client::describe_app(&app), crate::launch::describe_closed(how)),
                None if applet => "the Album wasn't open".to_string(),
                None => "no application was running".to_string(),
            })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Installed titles with their ids, most recently played first: the order of the home menu's row (12 tiles, then All Software).")]
    async fn list_apps(&self, Parameters(args): Parameters<ListAppsArgs>) -> CallToolResult {
        let result = async {
            let apps = self.client.list_apps(args.max.unwrap_or(proto::HOME_MENU_TILES as u16)).await?;
            let mut text = String::new();
            for (i, app) in apps.iter().enumerate() {
                writeln!(text, "{i}. {:016x} {}", app.id, if app.name.is_empty() { "(name unavailable)" } else { &app.name })?;
            }
            anyhow::Ok(if text.is_empty() { "no titles installed".to_string() } else { text })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Start an installed game (title) from the home menu, as a player would: finds its tile (only the home menu's row of 12 most recent titles), presses A and answers the user picker. Closes whatever application is running first, so ask before closing a game the user is playing. Game cards must be inserted; digital titles may need an online check this console can't pass. Then play with input and screenshot.")]
    async fn launch_title(&self, Parameters(args): Parameters<LaunchTitleArgs>) -> CallToolResult {
        let result = async {
            let (app, notes) = crate::launch::launch_title(&self.client, &args.title, args.user.unwrap_or(0)).await?;
            anyhow::Ok(format!("{}\n{} ({:016x}) is running", notes.join("\n"), app.name, app.id))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Open hbmenu, where homebrew runs. mode \"applet\" (default): the home menu's Album button, which Atmosphère runs hbmenu in: quick, no game involved (a running game stays suspended), but only a few hundred MB of memory, so it's for starting the Kickstand NRO or small tools, never games. mode \"full\": hbloader started in place of a title (the daemon arms Atmosphère's override config for that one launch, so no button needs holding), with the console's full memory, which games need; closes whatever application runs (ask before closing a game the user is playing), tries the host title that worked before or else each title on the home menu's row, 20 s or more each. If no title works, relay the error: the user may need to launch a title holding R themselves (with the override armed, R gives the game instead).")]
    async fn open_hbmenu(&self, Parameters(args): Parameters<OpenHbmenuArgs>) -> CallToolResult {
        let result = async {
            let mode = crate::launch::Mode::parse(args.mode.as_deref(), crate::launch::Mode::Applet)?;
            let opened = crate::launch::open_hbmenu(&self.client, mode).await?;
            let mut text = opened.notes.join("\n");
            match opened.host {
                Some(host) => write!(text, "\nhbmenu is open with full memory (host title {:016x} {})", host.id, host.name)?,
                None => write!(text, "\nhbmenu is open in applet mode")?,
            }
            anyhow::Ok(text)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Run homebrew: start an NRO (local_path from this PC, or sd_path already on the SD card) with args from hbmenu, and return its stdout so far. mode \"full\" (default) runs it with the console's full memory in a game's place (see open_hbmenu), closing whatever application runs (ask before closing a game the user is playing); games like PaperBoat and TTYDRecomp always need this. mode \"applet\" runs small tools quickly from the Album, and is refused for games. An NRO already running in that mode is closed first; then hbmenu's NetLoader receives it. The NRO keeps running after this returns; read more output with nro_output, see it with screenshot, stop it with close_app. Takes about 30 s plus the transfer.")]
    async fn run_nro(&self, Parameters(args): Parameters<RunNroArgs>) -> CallToolResult {
        let result = async {
            let program = match (args.local_path, args.sd_path) {
                (Some(local), None) => {
                    let nro = tokio::fs::read(&local).await.with_context(|| format!("reading {local}"))?;
                    let name = std::path::Path::new(&local).file_name().context("local_path has no file name")?.to_string_lossy().into_owned();
                    let dest = args.dest.unwrap_or_else(|| format!("/switch/kickstand/{name}"));
                    crate::launch::Program::Local { nro, dest }
                }
                (None, Some(sd)) => crate::launch::Program::Sd(sd),
                _ => anyhow::bail!("give exactly one of local_path and sd_path"),
            };
            let symbols = match &args.elf {
                Some(elf) => Some(crate::symbolize::Symbolizer::new(elf.as_ref(), args.module.as_deref())?.shared()),
                None => None,
            };
            if let Some(elf) = &args.elf {
                *self.elf.lock().unwrap() = Some((elf.clone(), args.module.clone()));
            }
            let mode = crate::launch::Mode::parse(args.mode.as_deref(), crate::launch::Mode::Full)?;
            let started = crate::launch::run_nro(&self.client, program, args.args.unwrap_or_default(), mode).await?;
            let mut text = started.notes.join("\n");
            if let Some(stream) = started.output {
                let handle = crate::launch::OutputLog::collect(&self.output, stream, symbols, started.pid);
                if let Some(old) = self.collector.lock().unwrap().replace(handle) {
                    old.abort();
                }
                let wait = args.wait_seconds.unwrap_or(5).min(60);
                text.push_str(&self.read_output(wait).await);
            }
            anyhow::Ok(text)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Turn `<module>+0x<offset>` addresses in any text (a saved log, a backtrace) into function names and source lines, using the module's ELF on this PC. Bare hex offsets, one per line, work too.")]
    async fn symbolize(&self, Parameters(args): Parameters<SymbolizeArgs>) -> CallToolResult {
        let result = (|| {
            let symbols = crate::symbolize::Symbolizer::new(args.elf.as_ref(), args.module.as_deref())?;
            let mut out = String::new();
            for line in args.text.lines() {
                let bare = line.trim().strip_prefix("0x").unwrap_or(line.trim());
                match u64::from_str_radix(bare, 16) {
                    Ok(offset) if !bare.is_empty() => writeln!(out, "{line}\n        -> {}", symbols.lookup(offset))?,
                    _ => out.push_str(&symbols.annotate(&format!("{line}\n"))),
                }
            }
            anyhow::Ok(out)
        })();
        result.map_or_else(fail, ok)
    }

    #[tool(description = "List the newest crash reports (a process crashed: game, homebrew, or system process) and fatal records (the whole console stopped on Atmosphère's fatal screen), newest first, one line each. Check this after an NRO dies unexpectedly or the console stops answering. Times are the console's clock.")]
    async fn crash_reports(&self, Parameters(args): Parameters<CrashReportsArgs>) -> CallToolResult {
        let result = async {
            let entries = crate::crash::list(&self.client, args.limit.unwrap_or(5)).await?;
            let mut text = String::new();
            for e in &entries {
                writeln!(text, "{}\n    {}", e.path, e.headline)?;
            }
            anyhow::Ok(if text.is_empty() { "no crash reports or fatal records".to_string() } else { text })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Read one crash report (summarized: result, process, exception, the crashed thread's registers, backtrace and modules; with elf, its addresses become functions and source lines) or decode a fatal record (program, error, PC, a kernel result in X0). Includes the screenshot Atmosphère saved with the crash, if any. Homebrew NROs appear as raw addresses (hbloader loads them, so Atmosphère can't name them); their own printed backtraces in run_nro output are symbolized instead.")]
    async fn crash_report(&self, Parameters(args): Parameters<CrashReportArgs>) -> CallToolResult {
        let result = async {
            let fetched = crate::crash::fetch(&self.client, &args.name).await?;
            let symbols = match &args.elf {
                Some(elf) => Some(crate::symbolize::Symbolizer::new(elf.as_ref(), args.module.as_deref())?),
                None => None,
            };
            let text = crate::crash::describe(&fetched, symbols.as_ref())?;
            let mut content = vec![ContentBlock::text(text)];
            if let Some(jpg) = fetched.jpg {
                content.push(ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(jpg), "image/jpeg"));
            }
            anyhow::Ok(CallToolResult::success(content))
        };
        result.await.unwrap_or_else(fail)
    }

    #[tool(description = "Build, deploy, run and watch a project in one call (projects are set up in ~/.config/kickstand/projects.json; list them with the projects tool): runs its build command on this PC (a failure returns the end of the build log), checks the NRO was made from the ELF, sends it to its place on the SD card and runs it with full memory, then watches it like supervise. Returns the build result, what happened (exited, crashed with a symbolized report, ended, fatal, looks hung with backtraces, or still running), the output's last lines and a screenshot when there is one. Closes whatever runs first: don't use it while the user is playing. Its output stays readable with nro_output, and memory tools use its ELF afterwards.")]
    async fn test_build(&self, Parameters(args): Parameters<TestBuildArgs>) -> CallToolResult {
        let result = async {
            let project = crate::project::get(&args.project)?;
            let settings = crate::watchdog::Settings {
                max: std::time::Duration::from_secs(args.seconds.unwrap_or(60).min(300)),
                quiet: std::time::Duration::from_secs(args.quiet_seconds.unwrap_or(20).max(1)),
                reboot_on_fatal: true,
            };
            let run = crate::project::test(&self.client, &project, args.skip_build.unwrap_or(false), args.args, &self.output, &settings).await?;
            if let Some(handle) = run.collector
                && let Some(old) = self.collector.lock().unwrap().replace(handle)
            {
                old.abort();
            }
            *self.elf.lock().unwrap() = Some((run.elf.0, Some(run.elf.1)));
            let mut content = vec![ContentBlock::text(run.text)];
            if let Some(jpeg) = run.screenshot {
                content.push(ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(&jpeg), "image/jpeg"));
            }
            anyhow::Ok(content)
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "The projects test_build knows (from ~/.config/kickstand/projects.json): name, folder, build command, NRO and where it goes on the SD card.")]
    async fn projects(&self) -> CallToolResult {
        let result = async {
            let mut out = String::new();
            for (name, p) in crate::project::load_all()? {
                writeln!(out, "{name}: {} (build: {}; {} -> {})", p.dir.display(), p.build.as_deref().unwrap_or("none"), p.nro.display(), p.dest)?;
            }
            anyhow::Ok(if out.is_empty() { "no projects".to_string() } else { out })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Watch the running homebrew (after run_nro) until something happens, and say what: EXITED (with its last output), CRASHED (a new crash report for it, symbolized, with Atmosphère's screenshot), FATAL (the console stopped; rebooted into hekate entry 1 unless reboot_on_fatal is false), LOOKS HUNG (no output for quiet_seconds and a screen frozen for 5 s more: every thread's backtrace and a screenshot), or STILL RUNNING when seconds run out. Use it after starting a build to test, instead of polling status.")]
    async fn supervise(&self, Parameters(args): Parameters<SuperviseArgs>) -> CallToolResult {
        let result = async {
            let elf = self.elf(args.elf, None)?;
            let settings = crate::watchdog::Settings {
                max: std::time::Duration::from_secs(args.seconds.unwrap_or(60).min(300)),
                quiet: std::time::Duration::from_secs(args.quiet_seconds.unwrap_or(20).max(1)),
                reboot_on_fatal: args.reboot_on_fatal.unwrap_or(true),
            };
            let output = self.output.lock().unwrap().is_open().then_some(&self.output);
            let report = crate::watchdog::supervise(&self.client, output, &elf, &settings).await?;
            let mut content = vec![ContentBlock::text(report.text)];
            if let Some(jpeg) = report.screenshot {
                content.push(ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(&jpeg), "image/jpeg"));
            }
            anyhow::Ok(content)
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Every thread's backtrace in the running homebrew: the daemon pauses it for a moment, reads each thread's registers and follows its frame pointers; addresses are named from the NRO's ELF, with source lines when it has DWARF. Use it when the homebrew hangs or stalls (a thread blocked in a wait, a lock, a GPU fence, or spinning), or to see what it's doing. The ELF must be the build that's running (a warning says when it isn't).")]
    async fn threads(&self, Parameters(args): Parameters<MemoryMapArgs>) -> CallToolResult {
        let result = async {
            let elf = self.elf(args.elf, None)?;
            crate::memory::threads(&self.client, &elf).await
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "The running application's memory map: each region's range, size, permissions and kind, and where the homebrew's NRO is (nro+0x… addresses and the ELF's symbols are relative to it). Use it to see what read_memory can reach.")]
    async fn memory_map(&self, Parameters(args): Parameters<MemoryMapArgs>) -> CallToolResult {
        let result = async {
            let elf = self.elf(args.elf, None)?;
            crate::memory::map(&self.client, &elf).await
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Read the running application's memory while it runs (homebrew or a game): a variable by its symbol from the NRO's ELF (e.g. gPlayerStatus, with format f32 for floats), a struct field (symbol+0x…), a pointer chain ([symbol]+0x…), or any address from memory_map. Shows a hexdump or typed values; pointers into the NRO are named. Each call stops the application for a moment (the daemon attaches as its debugger and detaches), so one read is consistent, but values move between calls.")]
    async fn read_memory(&self, Parameters(args): Parameters<ReadMemoryArgs>) -> CallToolResult {
        let result = async {
            let elf = self.elf(args.elf, args.module)?;
            let format = crate::memory::Format::parse(args.format.as_deref().unwrap_or("hex"))?;
            crate::memory::peek(&self.client, &elf, &args.address, args.length.unwrap_or(0), format).await
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Write to the running homebrew's memory (refused for games): set a variable to test a fix or reach a state quickly (e.g. a flag, a position, a debug toggle). Takes an address expression like read_memory and returns the value before and after. Writing code or pointers can crash the NRO; check status and crash_reports if it stops.")]
    async fn write_memory(&self, Parameters(args): Parameters<WriteMemoryArgs>) -> CallToolResult {
        let result = async {
            let elf = self.elf(args.elf, args.module)?;
            let format = crate::memory::Format::parse(&args.format)?;
            crate::memory::poke(&self.client, &elf, &args.address, &args.value, format).await
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "More stdout from the NRO run_nro last started: what arrived since the last read, and whether it's still running (its stdout open).")]
    async fn nro_output(&self, Parameters(args): Parameters<NroOutputArgs>) -> CallToolResult {
        let wait = args.wait_seconds.unwrap_or(0).min(60);
        ok(self.read_output(wait).await.trim_start().to_string())
    }

    #[tool(description = "Keep the Switch awake (no dimming or auto-sleep), shown by a Kickstand icon next to its clock. Every tool call keeps it awake for 30 minutes, after which the console's own timers apply again to save battery. Call this with no arguments to renew those 30 minutes before a long stretch without tool calls (a big build, a long think), or with more minutes (up to 480) to watch a game run; minutes: 0 lets it sleep when you're done. A console that falls asleep can't be woken remotely, so renew rather than risk it. Not kept across reboots; after a reboot to an entry, the sysmodule gets past the lock screen itself. Changes no system settings.")]
    async fn keep_awake(&self, Parameters(args): Parameters<KeepAwakeArgs>) -> CallToolResult {
        let result = async {
            let s = self.client.keep_awake(Some(args.minutes.unwrap_or(DEFAULT_KEEP_AWAKE_MINUTES))).await?;
            anyhow::Ok(if s == 0 { "keep awake is off".to_string() } else { format!("screen stays on for {}m {}s", s / 60, s % 60) })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Take a screenshot of the Switch's screen and look at it (JPEG, 1280x720). Use width to look at a smaller copy when you only need to see what's on screen, and region to read part of it closely. The kickstand indicator isn't in it. Optionally saves the full image on this PC.")]
    async fn screenshot(&self, Parameters(args): Parameters<ScreenshotArgs>) -> CallToolResult {
        let result = async {
            let region = args.region.as_deref().map(parse_region).transpose()?;
            let jpeg = self.client.screenshot().await?;
            let mut text = format!("screenshot: {} bytes", jpeg.len());
            if let Some(path) = &args.local_path {
                tokio::fs::write(path, &jpeg).await?;
                write!(text, ", saved to {path}")?;
            }
            let jpeg = tokio::task::spawn_blocking(move || crate::capture::reframe(&jpeg, region, args.width)).await??;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
            anyhow::Ok(vec![ContentBlock::text(text), ContentBlock::image(b64, "image/jpeg")])
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Wait for the Switch's screen to change (a load to finish, a menu or dialog to appear, the game to react to input) or to stop changing (a transition, fade or scrolling text to end), then look at it: one call instead of polling with screenshots. Returns the screen when that happens, or when the timeout passes (and says which).")]
    async fn wait_screen(&self, Parameters(args): Parameters<WaitScreenArgs>) -> CallToolResult {
        let result = async {
            let until = match args.until.as_deref().unwrap_or("change") {
                "change" | "changed" => crate::capture::Until::Change,
                "still" | "stable" | "settle" => crate::capture::Until::Still,
                other => anyhow::bail!("until is \"change\" or \"still\", not {other:?}"),
            };
            let region = args.region.as_deref().map(parse_region).transpose()?;
            let timeout = args.timeout_seconds.unwrap_or(10.0).clamp(0.0, crate::capture::MAX_WAIT_SECONDS);
            let waited = crate::capture::wait(&self.client, until, region, std::time::Duration::from_secs_f64(timeout)).await?;
            let text = format!(
                "{} after {:.1} s ({} captures)",
                match (waited.met, until) {
                    (true, crate::capture::Until::Change) => "the screen changed",
                    (true, crate::capture::Until::Still) => "the screen is still",
                    (false, crate::capture::Until::Change) => "timed out: the screen didn't change",
                    (false, crate::capture::Until::Still) => "timed out: the screen kept changing",
                },
                waited.elapsed.as_secs_f64(),
                waited.captures
            );
            let width = args.width.unwrap_or(640);
            let jpeg = tokio::task::spawn_blocking(move || crate::capture::reframe(&waited.jpeg, None, Some(width))).await??;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
            anyhow::Ok(vec![ContentBlock::text(text), ContentBlock::image(b64, "image/jpeg")])
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Record the Switch's screen as a looping GIF (from repeated screenshots, a few frames a second) saved on this PC (by default in ~/Pictures/kickstand/), e.g. to show the user an animation or a bug. Returns a contact sheet of up to 6 evenly spaced frames so you can see the motion yourself.")]
    async fn record_gif(&self, Parameters(args): Parameters<RecordGifArgs>) -> CallToolResult {
        let result = async {
            let seconds = args.seconds.unwrap_or(3.0);
            let width = args.width.unwrap_or(640).clamp(64, 1280);
            let shots = crate::capture::record(&self.client, seconds, args.fps.unwrap_or(5)).await?;
            let span = shots.last().map_or(0.0, |s| s.at.as_secs_f64());
            let frames = shots.len();
            let (gif, sheet) = tokio::task::spawn_blocking(move || {
                anyhow::Ok((crate::capture::gif(&shots, width)?, crate::capture::contact_sheet(&shots, 6)?))
            })
            .await??;
            let path = match args.local_path {
                Some(p) => std::path::PathBuf::from(p),
                None => default_capture_path("gif")?,
            };
            tokio::fs::write(&path, &gif).await?;
            let fps = if span > 0.0 { (frames - 1) as f64 / span } else { 0.0 };
            let text = format!("saved {} ({frames} frames, {fps:.1} fps, {} KB); contact sheet below", path.display(), gif.len() / 1024);
            let b64 = base64::engine::general_purpose::STANDARD.encode(&sheet);
            anyhow::Ok(vec![ContentBlock::text(text), ContentBlock::image(b64, "image/jpeg")])
        };
        result.await.map_or_else(fail, CallToolResult::success)
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

    #[tool(description = "Manage the kickstand sysmodule, the always-on daemon: status, install (upload a built .nsp and restart onto it without a reboot, which opens and closes hbmenu in applet mode; don't do it while the user is using the console), start, stop, or handover (start it and end the hbmenu NRO daemon). Never enables start-at-boot; that needs the user's go-ahead.")]
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
                    let nro = match args.nro_path {
                        Some(path) => Some(tokio::fs::read(&path).await.with_context(|| format!("reading {path}"))?),
                        None => None,
                    };
                    self.client.install_sysmodule(&nsp).await?;
                    if args.restart.unwrap_or(true) {
                        let notes = crate::launch::restart_sysmodule(&self.client, nro).await?;
                        let status = self.client.sysmodule(proto::Request::SysmoduleStatus).await?;
                        return anyhow::Ok(format!("{}\n{}", notes.join("\n"), describe(&status)));
                    }
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
    instructions = "Tools for a real Nintendo Switch running the kickstand daemon. Paths are absolute from the SD card root. Start with ping. The console stays awake for 30 minutes after each tool call and can't be woken remotely once asleep: call keep_awake before going longer than that without a tool call, and keep_awake with minutes 0 when finished. To run homebrew, use run_nro (it opens hbmenu itself) and read its stdout with nro_output; status says where the console is (what runs, the lock screen after a reboot, keep-awake) with a small screenshot."
)]
impl ServerHandler for Tools {}
