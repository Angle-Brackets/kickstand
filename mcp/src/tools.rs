// SPDX-License-Identifier: GPL-3.0-only
//! The MCP tools. Each one is a thin wrapper over [`Client`]; failures come back as tool errors
//! (`isError: true`) with the daemon's message, so the agent can read and react to them.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::Context as _;
use base64::Engine;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock};
use rmcp::service::{ElicitationError, RequestContext};
use rmcp::{Peer, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::access::{self, Access, AllowFor, Approval, Need};
use crate::modalities::{Modalities, ModelRegistry, PingArgs, VisualFormat};
use crate::questions::Answer;
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
    /// The last screenshots handed out, by number, for input_until to cut patches from.
    shots: Arc<std::sync::Mutex<Shots>>,
    /// The folders the user allowed during this session (see access.rs).
    access: Arc<Access>,
    /// Access questions put to the user outside the AI app (its window, and the chat), not answered yet.
    asking: Arc<std::sync::Mutex<Vec<Asking>>>,
    /// Paths the user allowed just once from the chat ([`Tools::answer_access`]), until they're used.
    once: Arc<std::sync::Mutex<Vec<(String, Need)>>>,
    /// Per-session AI model and modality registry populated by `ping`.
    models: ModelRegistry,
    /// Rolling in-memory video frame ring buffer.
    video_ring: Arc<std::sync::Mutex<crate::capture::VideoRingBuffer>>,
    tool_router: ToolRouter<Self>,
}

/// An access question the AI app couldn't show, so Kickstand asked in its window (`id`, if that opened)
/// and the agent is to ask in the chat.
struct Asking {
    id: Option<String>,
    path: String,
    folder: String,
    /// `folder` is the file itself (one at the card's root).
    file: bool,
    need: Need,
    action: String,
}

/// Screenshots kept for [`Tools::input_until`], newest last.
#[derive(Default)]
struct Shots {
    next: u64,
    kept: std::collections::VecDeque<(u64, Vec<u8>)>,
}

/// How many screenshots [`Shots`] keeps.
const SHOTS_KEPT: usize = 12;

/// An ELF path and the module name its addresses use.
type ElfChoice = (String, Option<String>);

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PathArg {
    /// Absolute path from the SD card root, e.g. /switch or /atmosphere/crash_reports.
    pub path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TailArgs {
    /// A log file on the SD card, absolute from its root.
    pub path: String,
    /// Without from: how many of its last lines to return first (default 40).
    pub lines: Option<u32>,
    /// Continue from this byte offset (the "next" a previous call returned) instead of the last lines.
    pub from: Option<u64>,
    /// Then keep collecting what's added for this long (default 0, max 60).
    pub follow_seconds: Option<f64>,
    /// Stop following as soon as a new line contains this text.
    pub until: Option<String>,
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
    /// Destination on the SD card, absolute from its root, e.g. /switch/mygame/mygame.nro.
    pub remote_path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DeleteArgs {
    /// File or directory on the SD card, absolute from its root.
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
    /// Then look, in this call: "still" waits for the screen to settle (a menu moving, a fade) and returns
    /// it, "change" waits for it to change; as wait_screen, up to 10 s.
    pub then: Option<String>,
    /// Width of that screenshot (default 640).
    pub width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct InputUntilArgs {
    /// Hold this the whole time: buttons and sticks joined with "+" as in input's steps, without ":MS",
    /// e.g. "right", "l=1,0" (left stick right), "l=1,0+b" (run right), "zl+a".
    pub hold: Option<String>,
    /// Or press these together over and over (e.g. "a" to get through dialogue), every every_ms.
    pub repeat: Option<String>,
    /// Milliseconds between repeat presses (default 400, at least 150).
    pub every_ms: Option<u32>,
    /// Or play these input steps (input's syntax) over and over.
    pub steps: Option<String>,
    /// When to stop. "change": the screen (or region) differs from when it began. "still": it stops
    /// changing. "appears": the part of an earlier screenshot given by region (and shot) shows up within
    /// search. "gone": that part is no longer there.
    pub until: String,
    /// "x,y,w,h" in the 1280x720 screen. For change and still: only watch this part. For appears and gone:
    /// the part of the screenshot to look for (at least 8x8; something distinctive, like a prompt or an
    /// icon, 40-200 pixels across). Read it off a screenshot taken with grid: true.
    pub region: Option<String>,
    /// For appears and gone: the screenshot (its number, from screenshot or wait_screen) to take region
    /// from; default the newest.
    pub shot: Option<u64>,
    /// For appears and gone: where to look, "x,y,w,h" or "screen" (default: around region, 96 pixels each
    /// way, so it may move a little).
    pub search: Option<String>,
    /// Give up after this many seconds (default 15, max 60).
    pub timeout_seconds: Option<f64>,
    /// Width of the screenshot you get back (default 640).
    pub width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct InputArgs {
    /// Space-separated steps, each WHAT:MS, held for MS milliseconds. WHAT joins with "+": button names,
    /// "l=X,Y" / "r=X,Y" for the left/right stick (-1.0..1.0, y up; centred if absent), "touch=X,Y" for a
    /// finger on the 1280x720 touch screen, or "-" for nothing (a pause). Held things stay held across
    /// consecutive steps. Examples: "r:300 r+a:150 r:6000" (R through a launch), "l=0,1:1500" (walk up
    /// 1.5 s), "l=1,0+b:800" (run right), "touch=640,360:100" (tap the centre). At most 64 steps, 20 s.
    pub steps: String,
    /// Then look, in this call: "still" or "change", as press's then.
    pub then: Option<String>,
    /// Width of that screenshot (default 640).
    pub width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RunNroArgs {
    /// An NRO on this PC to send and run (e.g. a fresh build).
    pub local_path: Option<String>,
    /// Where to put local_path on the SD card (default /switch/kickstand/<file name>). Must be under
    /// /switch/, in a directory that exists.
    pub dest: Option<String>,
    /// Instead of local_path: an NRO already on the SD card, e.g. /switch/mygame/mygame.nro.
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
    /// in the Album, a few hundred MB: small tools only, refused for the projects in projects.json).
    pub mode: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TestBuildArgs {
    /// A project from ~/.config/kickstand/projects.json, by name.
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
pub struct QuitAppArgs {
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
    /// Only lines matching this regular expression (Rust regex syntax), e.g. "report=|error".
    pub grep: Option<String>,
    /// With grep: also this many lines after each match (e.g. a report's first few entries).
    pub after: Option<usize>,
    /// At most this many lines, the newest (after grep).
    pub max_lines: Option<usize>,
    /// Start each line with when it arrived, in seconds since the NRO started (e.g. "[12.345s] ").
    pub timestamps: Option<bool>,
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
pub struct RecordMacroArgs {
    /// "start": record the physical controllers while the user plays (tell them first). "stop": stop and
    /// save it as name.
    pub action: String,
    /// The macro's name when stopping (letters, digits, spaces, - and _), e.g. "title to rogueport".
    pub name: Option<String>,
    /// When stopping: what it does and where it starts from ("from the title screen: file 1 to Rogueport's
    /// square"), for whoever plays it later.
    pub note: Option<String>,
    /// When starting: stop by itself after this many seconds (default 300, max 600).
    pub max_seconds: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PlayMacroArgs {
    /// A saved macro's name (see macros).
    pub name: String,
    /// Then look, in the same call: "still" or "change", as press's then.
    pub then: Option<String>,
    /// Width of that screenshot (default 640).
    pub width: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SaveMacroArgs {
    /// Its name (letters, digits, spaces, - and _), e.g. "title to rogueport".
    pub name: String,
    /// The input, in input's step syntax, e.g. "a:100 -:2500 l=0,1:1800 a:100".
    pub steps: String,
    /// Where it starts from and what it does ("from the title screen: file 1 to Rogueport's square"), for
    /// whoever plays it later.
    pub note: String,
    /// Replace a macro already called name (default false). Only with the user's agreement if they
    /// recorded it.
    pub replace: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ControlMacroArgs {
    /// "pause", "resume" or "stop".
    pub action: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TypeTextArgs {
    /// What to type: printable ASCII, tabs and newlines, up to 250 characters (US keyboard layout).
    pub text: String,
    /// Press Enter afterwards (default false), e.g. to confirm the software keyboard.
    pub enter: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct FindArgs {
    /// The folder to search under, e.g. /switch.
    pub dir: String,
    /// Part of the name to look for, ignoring case (e.g. ".nro", "save"); empty lists everything.
    pub name: Option<String>,
    /// Most results (default 100, max 1000).
    pub max: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GrepArgs {
    /// A file, or a folder to search every file under (files over 16 MB and binary files skipped).
    pub path: String,
    /// The text to look for (plain text, not a pattern).
    pub text: String,
    /// Ignore ASCII case (default false).
    pub ignore_case: Option<bool>,
    /// Most matching lines (default 50, max 500).
    pub max: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AnswerAccessArgs {
    /// The path the request was about, as the tool said.
    pub path: String,
    /// What the user chose: "once" (just this once), "session" (this folder until Kickstand restarts),
    /// "always" (this folder from now on) or "no".
    pub answer: String,
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
    /// Draw a grid every 100 screen pixels, labelled with coordinates, to read positions off accurately
    /// (for regions in input_until, touch or region). Default false.
    pub grid: Option<bool>,
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
    /// MyGame+0x…), heap+0x…, a symbol from the ELF (e.g. gPlayerStatus), any of those plus or minus
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

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReloadNroArgs {
    /// Project name in ~/.config/kickstand/projects.json (optional if local_path or sd_path is given).
    pub project: Option<String>,
    /// Local NRO on this PC to upload and hot-reload.
    pub local_path: Option<String>,
    /// Destination on the SD card when uploading local_path (default: project's dest or /switch/kickstand/<name>).
    pub dest: Option<String>,
    /// NRO already on the SD card to hot-reload.
    pub sd_path: Option<String>,
    /// Arguments for the NRO (argv[1..]; default: project's args if project is set).
    pub args: Option<Vec<String>>,
    /// Run the project's build command before reloading when project is set (default false).
    pub build: Option<bool>,
    /// Upload local_path over USB instead of Wi-Fi when a USB cable or SD mount is present (default true).
    pub prefer_usb: Option<bool>,
    /// Graceful in-app exit button sequence (e.g. "plus") to try before falling back to hbmenu re-entry.
    pub exit_buttons: Option<String>,
    /// Optional saved macro name to replay automatically after reload completes to return to a test state.
    pub replay_macro: Option<String>,
    /// Use the app's next-load hook (the app chain-loads the new build itself; see TOOLS.md). Default:
    /// detected (the NRO contains "kickstand/next-load"); false never tries it, true always does.
    pub hook: Option<bool>,
    /// Seconds to collect stdout before returning (default 5, max 60).
    pub wait_seconds: Option<u32>,
    /// ELF path for symbolizing backtraces and subsequent memory/profile calls.
    pub elf: Option<String>,
    /// Module name for `<module>+0x<offset>` backtraces.
    pub module: Option<String>,
    /// "full" (default) or "applet".
    pub mode: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProfileArgs {
    /// Sampling duration in seconds (default 3.0, range 0.1..=15.0).
    pub seconds: Option<f64>,
    /// Sampling rate in Hz (default 50, range 5..=100).
    pub hz: Option<u32>,
    /// Optional specific thread ID (`0x...` or decimal) to sample; omit to sample all application threads.
    pub thread_id: Option<String>,
    /// Path to the unstripped `.elf` on this PC for DWARF function/file:line symbolization (defaults to the
    /// last ELF passed to `run_nro`, `test_build`, or a memory tool).
    pub elf: Option<String>,
    /// Module name for `<module>+0x...` (default: ELF file stem).
    pub module: Option<String>,
    /// Minimum inclusive percentage to display a node in the AI call tree (default 2.0%).
    pub min_tree_pct: Option<f64>,
    /// Maximum exclusive hotspots to list in the AI summary (default 15).
    pub max_hotspots: Option<usize>,
    /// Where to save the interactive `.svg` flamegraph on this PC (default:
    /// `~/Pictures/kickstand/svg-<unix time>.svg`).
    pub svg_path: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AudioReplayArgs {
    /// Seconds of recent audio to pull from the 5-second rolling ring buffer (0.1 to 5.0, default 5.0).
    pub seconds: Option<f64>,
    /// Where to save the `.wav` file on this PC (default: `~/Pictures/kickstand/wav-<unix time>.wav`).
    pub local_path: Option<String>,
    /// Optional path to a reference `.wav` file on this PC (e.g. an expected SFX) to cross-correlate
    /// against the captured audio.
    pub reference_wav: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecordAudioArgs {
    /// Seconds of live audio to record (0.1 to 30.0, default 3.0).
    pub seconds: Option<f64>,
    /// Where to save the `.wav` file on this PC (default: `~/Pictures/kickstand/wav-<unix time>.wav`).
    pub local_path: Option<String>,
    /// Optional path to a reference `.wav` file on this PC to cross-correlate against the recording.
    pub reference_wav: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AudioControlArgs {
    /// Set system output volume percentage (0 to 100). Omit to leave unchanged and only query status.
    pub volume: Option<u8>,
    /// Mute (`true`) or unmute (`false`) system audio output. Omit to leave unchanged.
    pub muted: Option<bool>,
}

/// `~/Pictures/kickstand/<ext>-<unix time>.<ext>`, creating the folder: where captures go when the agent
/// doesn't say, so the user knows where to look.
pub(crate) fn default_capture_path(ext: &str) -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME isn't set; pass local_path"))?;
    let dir = std::path::Path::new(&home).join("Pictures/kickstand");
    std::fs::create_dir_all(&dir)?;
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    Ok(dir.join(format!("{ext}-{secs}.{ext}")))
}

/// What `keep_awake` renews with no arguments: the daemon's per-request lease.
const DEFAULT_KEEP_AWAKE_MINUTES: u32 = 30;

impl Tools {
    /// Saves captured audio, analyses it and formats the reply for the session's modalities, `note` first.
    async fn audio_reply(
        &self,
        samples: Vec<i16>,
        note: String,
        local_path: Option<String>,
        reference_wav: Option<String>,
        context: &RequestContext<RoleServer>,
    ) -> anyhow::Result<Vec<ContentBlock>> {
        let wav_path = match local_path {
            Some(p) => std::path::PathBuf::from(p),
            None => default_capture_path("wav")?,
        };
        let ref_path = reference_wav.map(std::path::PathBuf::from);
        let modalities = self.session_modalities(context);
        let (_, _, mut blocks) = tokio::task::spawn_blocking(move || {
            crate::audio::format_audio_output(
                &samples,
                crate::audio::DEFAULT_SAMPLE_RATE,
                crate::audio::DEFAULT_CHANNELS,
                &wav_path,
                ref_path.as_deref(),
                modalities,
            )
        })
        .await??;
        blocks.insert(0, ContentBlock::text(note));
        Ok(blocks)
    }

    /// Streams `seconds` of the console's own gameplay video next to `near` (an .mp4 via ffmpeg). Returns
    /// the file, its frame rate, frame count and size in KB; errors say why it couldn't (for the fallback).
    async fn gameplay_video(&self, seconds: f64, near: &std::path::Path) -> anyhow::Result<(std::path::PathBuf, f64, usize, usize)> {
        crate::video::record_to_file(&self.client, seconds, near).await
    }

    fn session_key(&self, context: &RequestContext<RoleServer>) -> String {
        context
            .peer
            .peer_info()
            .map(|info| info.client_info.name.clone())
            .or_else(|| context.meta.client_info().map(|info| info.name))
            .or_else(|| self.client.app())
            .unwrap_or_else(|| "default".to_string())
    }

    pub(crate) fn session_modalities(&self, context: &RequestContext<RoleServer>) -> Modalities {
        self.models.modalities_or_default(&self.session_key(context))
    }

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
    /// Waits for the screen as wait_screen does and returns it, after `done` (what the call did first).
    async fn look(&self, done: &str, until: crate::capture::Until, region: Option<(u32, u32, u32, u32)>, timeout: std::time::Duration, width: u32) -> anyhow::Result<Vec<ContentBlock>> {
        use crate::capture::Until;
        let waited = crate::capture::wait(&self.client, until, region, timeout).await?;
        let what = match (waited.met, until) {
            (true, Until::Change) => "the screen changed",
            (true, Until::Still) => "the screen is still",
            (false, Until::Change) => "timed out: the screen didn't change",
            (false, Until::Still) => "timed out: the screen kept changing",
        };
        let id = self.remember(&waited.jpeg);
        let text = format!("{done}{}{what} after {:.1} s ({} captures) (shot {id})", if done.is_empty() { "" } else { "; " }, waited.elapsed.as_secs_f64(), waited.captures);
        let jpeg = scaled(waited.jpeg, width).await?;
        Ok(vec![ContentBlock::text(text), jpeg_block(&jpeg)])
    }

    /// press and input's `then`: the input went through either way, so a screen that can't be read is
    /// reported, not an error.
    async fn look_after(&self, done: &str, until: crate::capture::Until, width: Option<u32>) -> Vec<ContentBlock> {
        match self.look(done, until, None, THEN_TIMEOUT, width.unwrap_or(640)).await {
            Ok(content) => content,
            Err(e) => vec![ContentBlock::text(format!("{done}; then couldn't look at the screen: {e:#}"))],
        }
    }

    async fn read_output(&self, wait_s: u32, view: &OutputView) -> String {
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(u64::from(wait_s));
        while self.output.lock().unwrap().is_open() && tokio::time::Instant::now() < until {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        let new = self.output.lock().unwrap().take_new();
        let status = match &new.end {
            _ if new.open => "the NRO is running".to_string(),
            Some(end) => end.describe(),
            None => "the NRO's stdout is closed".to_string(),
        };
        if new.lines.is_empty() {
            return format!("\n[no new output; {status}]");
        }
        let (shown, notes) = view.pick(&new.lines);
        let mut text = String::from("\n--- output ---\n");
        if new.dropped > 0 {
            let _ = writeln!(text, "[{} earlier bytes dropped]", new.dropped);
        }
        for i in shown {
            let (at, line) = &new.lines[i];
            if view.timestamps {
                let _ = write!(text, "[{at:.3}s] ");
            }
            text.push_str(line);
            text.push('\n');
        }
        let notes: Vec<String> = notes.into_iter().chain([status]).collect();
        let _ = write!(text, "[{}]", notes.join("; "));
        text
    }
}

/// Which lines of new NRO output [`Tools::nro_output`] shows, and how.
#[derive(Default)]
struct OutputView {
    grep: Option<regex::Regex>,
    /// Lines shown after each match.
    after: usize,
    /// Show at most this many (the newest).
    max_lines: Option<usize>,
    /// Start lines with when they arrived.
    timestamps: bool,
}

impl OutputView {
    /// The indices of the lines to show, and notes on what was left out.
    fn pick(&self, lines: &[(f64, String)]) -> (Vec<usize>, Vec<String>) {
        let mut notes = Vec::new();
        let mut shown: Vec<usize> = match &self.grep {
            None => (0..lines.len()).collect(),
            Some(re) => {
                let mut shown = Vec::new();
                let mut until = 0;
                for (i, (_, line)) in lines.iter().enumerate() {
                    if re.is_match(line) {
                        until = i + 1 + self.after;
                    }
                    if i < until {
                        shown.push(i);
                    }
                }
                notes.push(format!("{} of {} new lines: grep {:?}{}", shown.len(), lines.len(), re.as_str(), if self.after > 0 { format!(" with {} after each", self.after) } else { String::new() }));
                shown
            }
        };
        if let Some(max) = self.max_lines
            && shown.len() > max
        {
            let cut = shown.len() - max;
            shown.drain(..cut);
            notes.push(format!("the {cut} before these left out (max_lines {max})"));
        }
        (shown, notes)
    }
}

/// How long press and input's `then` waits for the screen.
const THEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// wait_screen's `until`, and press and input's `then`.
fn parse_until(until: &str) -> anyhow::Result<crate::capture::Until> {
    Ok(match until {
        "change" | "changed" => crate::capture::Until::Change,
        "still" | "stable" | "settle" => crate::capture::Until::Still,
        other => anyhow::bail!("expected \"change\" or \"still\", not {other:?}"),
    })
}

/// "x,y,w,h".
fn parse_region(r: &str) -> anyhow::Result<(u32, u32, u32, u32)> {
    let n: Vec<u32> = r.split(',').map(|v| v.trim().parse()).collect::<Result<_, _>>().context("region is x,y,w,h")?;
    let [x, y, w, h] = n[..] else { anyhow::bail!("region is x,y,w,h") };
    Ok((x, y, w, h))
}

/// A JPEG as an image block.
fn jpeg_block(jpeg: &[u8]) -> ContentBlock {
    ContentBlock::image(base64::engine::general_purpose::STANDARD.encode(jpeg), "image/jpeg")
}

/// `jpeg` scaled to `width` pixels, off the async threads.
async fn scaled(jpeg: Vec<u8>, width: u32) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || crate::capture::reframe(&jpeg, None, Some(width))).await?
}

/// The watchdog's limits from the `seconds` / `quiet_seconds` arguments shared by test_build and supervise.
fn watch_settings(seconds: Option<u64>, quiet_seconds: Option<u64>, reboot_on_fatal: bool) -> crate::watchdog::Settings {
    crate::watchdog::Settings {
        max: std::time::Duration::from_secs(seconds.unwrap_or(60).min(300)),
        quiet: std::time::Duration::from_secs(quiet_seconds.unwrap_or(20).max(1)),
        reboot_on_fatal,
    }
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
        Tools {
            client,
            output: Default::default(),
            collector: Default::default(),
            elf: Default::default(),
            shots: Default::default(),
            access: Default::default(),
            asking: Default::default(),
            once: Default::default(),
            models: Default::default(),
            video_ring: Default::default(),
            tool_router: Self::tool_router(),
        }
    }

    /// Keeps `jpeg` (full size) and returns its number.
    fn remember(&self, jpeg: &[u8]) -> u64 {
        let mut shots = self.shots.lock().unwrap();
        shots.next += 1;
        let id = shots.next;
        shots.kept.push_back((id, jpeg.to_vec()));
        while shots.kept.len() > SHOTS_KEPT {
            shots.kept.pop_front();
        }
        id
    }

    /// Screenshot `id` (the newest if `None`).
    fn shot(&self, id: Option<u64>) -> anyhow::Result<(u64, Vec<u8>)> {
        let shots = self.shots.lock().unwrap();
        let found = match id {
            Some(id) => shots.kept.iter().find(|(n, _)| *n == id),
            None => shots.kept.back(),
        };
        match (found, id) {
            (Some((n, jpeg)), _) => Ok((*n, jpeg.clone())),
            (None, Some(id)) => anyhow::bail!("shot {id} is gone (the last {SHOTS_KEPT} are kept): take a screenshot and use its number"),
            (None, None) => anyhow::bail!("no screenshot yet: take one and pick the region from it"),
        }
    }

    /// Lets the agent `action` (e.g. "read /config/x.ini") on `path` (normalized) only inside the folders
    /// it may use freely or the user allows now: the AI app shows the user the question.
    async fn permit(&self, peer: &Peer<RoleServer>, path: &str, need: Need, is_dir: bool, action: &str) -> anyhow::Result<()> {
        if self.access.allowed(path, need) {
            return Ok(());
        }
        {
            let mut once = self.once.lock().unwrap();
            if let Some(i) = once.iter().position(|(p, n)| p == path && (*n == need || *n == Need::Write)) {
                once.remove(i);
                return Ok(());
            }
        }
        let folder = access::folder_of(path, is_dir);
        let file = access::is_file_scope(path, is_dir);
        let message = format!(
            "Kickstand: the agent wants to {action} on your Switch's SD card. Allowing it covers {}.",
            covers(&folder, file, need)
        );
        match peer.elicit_with_timeout::<Approval>(message, Some(std::time::Duration::from_secs(300))).await {
            Ok(Some(answer)) => match answer.allow {
                AllowFor::Once => Ok(()),
                AllowFor::Session => {
                    self.access.allow_for_session(&folder, need);
                    Ok(())
                }
                AllowFor::Always => access::save(&folder, need),
            },
            Ok(None) | Err(ElicitationError::UserDeclined | ElicitationError::UserCancelled) => {
                anyhow::bail!("the user said no to letting you {action}. Don't look for another way; if you need it, ask them why it's needed")
            }
            // This AI app can't show the question: Kickstand asks in its own window, and the agent in the chat.
            Err(ElicitationError::CapabilityNotSupported) => self.ask_elsewhere(peer, path, &folder, file, need, action),
            Err(e) => anyhow::bail!("couldn't ask the user about {path}: {e}"),
        }
    }

    /// Asks about `path` in Kickstand's window and has the agent ask in the chat; whichever answer comes
    /// first counts ([`Tools::answer_access`] records a chat answer and takes the window's question away).
    fn ask_elsewhere(&self, peer: &Peer<RoleServer>, path: &str, folder: &str, file: bool, need: Need, action: &str) -> anyhow::Result<()> {
        let covers = covers(folder, file, need);
        let it = if file { "this file" } else { "this folder" };
        let flag = if need == Need::Write { " --write" } else { "" };
        let in_chat = format!(
            "Ask the user here, as a question (with your app's question tool if it has one), whether you may {action}; \
             allowing it would cover {covers}. Offer: just this once, {it} until Kickstand restarts, {it} from \
             now on, or no. Then pass their answer to answer_access with path {path:?} and try again. If they \
             say they answered in Kickstand's window instead, just try again. (They can also run `kickstand allow {folder}{flag}` in \
             their own terminal.)"
        );
        let pending = self.asking.lock().unwrap().iter().find(|a| a.path == path && a.need == need).map(|a| a.id.clone());
        // The window and the chat ask at the same time; a retry picks up an answer given in the window.
        let (answer, window_open) = match pending {
            Some(id) => (id.as_deref().and_then(crate::questions::answered), id.is_some()),
            None => {
                let app = self.client.app().or_else(|| peer.peer_info().map(|info| info.client_info.name.clone()));
                let id = crate::questions::post(app, action, &covers).ok();
                let window_open = id.is_some();
                self.asking.lock().unwrap().push(Asking { id, path: path.into(), folder: folder.into(), file, need, action: action.into() });
                (None, window_open)
            }
        };
        let Some(answer) = answer else {
            let window = if window_open {
                "Kickstand is also asking the user in its desktop window (no answer there yet)."
            } else {
                "This AI app can't show Kickstand's question and Kickstand's window couldn't open."
            };
            anyhow::bail!("{path} is outside the folders Kickstand may {} without asking. {window} {in_chat}", need.verb());
        };
        self.take_asking(path);
        self.apply(answer, folder, need, action)
    }

    /// Lets the agent read the SD card paths an NRO is started with (`--log=sdmc:/game.log`) for the rest of
    /// this session: files it was told to write, which whoever started it needs to read back.
    fn readable_outputs(&self, args: &[String]) {
        for path in access::paths_in_args(args) {
            if proto::path::check_read(&path).is_ok() {
                self.access.allow_for_session(&path, Need::Read);
            }
        }
    }

    /// Drops the question about `path`, taking it out of Kickstand's window.
    fn take_asking(&self, path: &str) -> Option<Asking> {
        let mut asking = self.asking.lock().unwrap();
        let taken = asking.iter().position(|a| a.path == path).map(|i| asking.remove(i));
        if let Some(id) = taken.as_ref().and_then(|a| a.id.as_deref()) {
            crate::questions::withdraw(id);
        }
        taken
    }

    /// Acts on the user's answer about `folder`; `Ok` lets the request go ahead.
    fn apply(&self, answer: Answer, folder: &str, need: Need, action: &str) -> anyhow::Result<()> {
        match answer {
            Answer::Once => Ok(()),
            Answer::Session => {
                self.access.allow_for_session(folder, need);
                Ok(())
            }
            Answer::Always => access::save(folder, need),
            Answer::No => anyhow::bail!("the user said no to letting you {action}. Don't look for another way; if you need it, ask them why it's needed"),
        }
    }

    #[tool(description = "Pass on the user's answer to Kickstand's access question when they gave it in this chat: only after a tool said Kickstand is asking (its window shows the same question, which this takes away), and only what the user actually chose; never answer for them. Then try the request again.")]
    async fn answer_access(&self, Parameters(args): Parameters<AnswerAccessArgs>) -> CallToolResult {
        let result = async {
            let answer = match args.answer.as_str() {
                "once" => Answer::Once,
                "session" => Answer::Session,
                "always" => Answer::Always,
                "no" => Answer::No,
                other => anyhow::bail!("answer is once, session, always or no, not {other:?}"),
            };
            let path = proto::path::normalize(&args.path).map_err(|e| anyhow::anyhow!("{}", e.describe()))?;
            let Some(asked) = self.take_asking(&path) else {
                anyhow::bail!("Kickstand hasn't asked the user about {path}: make the request first, and pass on an answer only after Kickstand asks")
            };
            if answer == Answer::Once {
                self.once.lock().unwrap().push((path.clone(), asked.need));
                return Ok(format!("allowed once: try the request on {path} again"));
            }
            self.apply(answer, &asked.folder, asked.need, &asked.action)?;
            Ok(match answer {
                Answer::Session => format!("{} allowed until Kickstand restarts: try again", access::describe(&asked.folder, asked.file)),
                _ => format!("{} allowed from now on: try again", access::describe(&asked.folder, asked.file)),
            })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "The macros saved on this PC: controller input the user recorded once (in Kickstand's desktop app, with `kickstand macro record`, or through record_macro) for agents to replay with play_macro, e.g. to get from the title screen to a level. Each with its length, when it was recorded and what it does.")]
    async fn macros(&self) -> CallToolResult {
        let result = async {
            let all = crate::macros::list()?;
            if all.is_empty() {
                return anyhow::Ok("no macros yet: the user can record one in Kickstand's desktop app (Macros), with `kickstand macro record NAME`, or you can ask them to play while you call record_macro, or write one yourself with save_macro".to_string());
            }
            let mut text = String::new();
            for m in all {
                write!(text, "{}: {}, {} steps", m.name, crate::macros::length(m.data.ms), m.data.steps.split_whitespace().count())?;
                if let Some(by) = &m.data.by {
                    write!(text, ", written by {by}")?;
                }
                if !m.data.note.is_empty() {
                    write!(text, ". {}", m.data.note)?;
                }
                text.push('\n');
            }
            anyhow::Ok(text)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Record a macro from the physical controllers while the user plays: start, have the user play (tell them what to do, and to put the controller down when done), then stop with a name and a note. Records buttons and both sticks of every controller (not HOME, Capture or touch), up to 10 minutes. Only useful with someone at the console; replay it later with play_macro.")]
    async fn record_macro(&self, Parameters(args): Parameters<RecordMacroArgs>) -> CallToolResult {
        let result = async {
            match args.action.as_str() {
                "start" => {
                    let seconds = args.max_seconds.unwrap_or(300).clamp(1, proto::MAX_RECORD_MS / 1000);
                    self.client.record_start(seconds * 1000).await?;
                    anyhow::Ok(format!("recording for up to {seconds} s: the user can play now; call record_macro with action stop and a name when they're done"))
                }
                "stop" => {
                    let name = args.name.as_deref().context("give the macro a name to save it under")?;
                    let name = crate::macros::check_name(name)?;
                    let (inputs, ms, full) = self.client.record_stop().await?;
                    let data = crate::macros::from_recording(&inputs, ms, args.note.unwrap_or_default())?;
                    let path = crate::macros::save(name, &data)?;
                    let mut text = format!("saved macro {name:?} ({}, {} steps) to {}", crate::macros::length(data.ms), data.steps.split_whitespace().count(), path.display());
                    if full {
                        text.push_str(" (it stopped early: the recording was full)");
                    }
                    anyhow::Ok(text)
                }
                other => anyhow::bail!("action is \"start\" or \"stop\", not {other:?}"),
            }
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Replay a saved macro (see macros) on a virtual controller, with its recorded timing, e.g. to get back to the same place in a game for a test. Start it from where it was recorded (its note says where); timing can drift a little over a long one, so check where it ended (then: \"still\" returns the screen) and fix up with press or input_until. The user (or control_macro) can pause or stop it part way; the result says if it was stopped.")]
    async fn play_macro(&self, Parameters(args): Parameters<PlayMacroArgs>) -> CallToolResult {
        let result = async {
            let then = args.then.as_deref().map(parse_until).transpose()?;
            let replayed = crate::macros::play(&self.client, &args.name).await?;
            let mut done = replayed.summary();
            if replayed.stopped {
                done.push_str(". If you didn't stop it with control_macro, the user did (in Kickstand's desktop app or a terminal): ask them before playing it again.");
            }
            match then {
                Some(until) => anyhow::Ok(self.look_after(&done, until, args.width).await),
                None => Ok(vec![ContentBlock::text(done)]),
            }
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Save a macro you wrote, in input's step syntax, for replaying later with play_macro (by you, another agent, or the user from Kickstand's desktop app). Work the steps out with input first and check the screen, then save the same steps with a note saying where it starts. Give loading screens generous waits (\"-:3000\"): load times vary. Doesn't replace an existing macro unless replace is true. Nothing is sent to the console.")]
    async fn save_macro(&self, Parameters(args): Parameters<SaveMacroArgs>) -> CallToolResult {
        let result = async {
            let name = crate::macros::check_name(&args.name)?;
            let by = self.client.app().map_or_else(|| "an agent".to_string(), |app| crate::macros::player(Some(&app)));
            let data = crate::macros::from_steps(&args.steps, args.note.trim().to_string(), by)?;
            let path = crate::macros::create(name, &data, args.replace.unwrap_or(false))?;
            anyhow::Ok(format!(
                "saved macro {name:?} ({}, {} steps) to {}; play_macro plays it",
                crate::macros::length(data.ms),
                data.steps.split_whitespace().count(),
                path.display()
            ))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Pause, resume or stop the macro playing on the console now (from play_macro, the user's desktop app or a terminal, whoever plays it). Paused, nothing is held and the macro's clock stops until resumed; a pause over 10 minutes stops it. Stopping lets go of everything.")]
    async fn control_macro(&self, Parameters(args): Parameters<ControlMacroArgs>) -> CallToolResult {
        let result = async {
            let action = match args.action.as_str() {
                "pause" => proto::PlaybackAction::Pause,
                "resume" => proto::PlaybackAction::Resume,
                "stop" => proto::PlaybackAction::Stop,
                other => anyhow::bail!("action is \"pause\", \"resume\" or \"stop\", not {other:?}"),
            };
            // A connection of its own: this agent's play_macro may be holding the usual one.
            let was = self.client.another().playback(action).await?;
            anyhow::Ok(crate::macros::control_text(&was, action))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Type text on a virtual USB keyboard: into the software keyboard (naming a save, a profile, a search) or anything that reads a keyboard. Printable ASCII, tabs and newlines, up to 250 characters a call, at about 14 characters a second; enter: true presses Enter after. Open the keyboard first (with press) and check it took the text with screenshot.")]
    async fn type_text(&self, Parameters(args): Parameters<TypeTextArgs>) -> CallToolResult {
        let result = async {
            let mut text = args.text;
            if args.enter.unwrap_or(false) {
                text.push('\n');
            }
            self.client.type_text(&text).await?;
            anyhow::Ok(format!("typed {} characters", text.chars().count()))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Find files and folders by name under a folder on the SD card, searched on the console (no listing every folder over Wi-Fi). Skips what Kickstand never reads; stops after 20 s with what it has.")]
    async fn find(&self, Parameters(args): Parameters<FindArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let dir = check_path(&args.dir, false)?;
            let name = args.name.unwrap_or_default();
            self.permit(&context.peer, &dir, Need::Read, true, &format!("search {dir} for {name:?}")).await?;
            let (entries, truncated) = self.client.find(&dir, &name, args.max.unwrap_or(100).clamp(1, proto::MAX_FIND)).await?;
            let mut text = String::new();
            for e in &entries {
                if e.is_dir {
                    writeln!(text, "{}/", e.name)?;
                } else {
                    writeln!(text, "{}  {} bytes", e.name, e.size)?;
                }
            }
            write!(text, "[{} found{}]", entries.len(), if truncated { "; stopped early (max, time, or a folder over 1024 entries): narrow the search" } else { "" })?;
            anyhow::Ok(text)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Search a file, or every file under a folder, on the SD card for lines containing some text, on the console: the matching lines with file and line number, without downloading anything. For logs and configs; binary files and files over 16 MB are skipped.")]
    async fn grep(&self, Parameters(args): Parameters<GrepArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let is_dir = self.client.stat(&path).await.map(|e| e.is_dir).unwrap_or(false);
            self.permit(&context.peer, &path, Need::Read, is_dir, &format!("search {path} for {:?}", args.text)).await?;
            let (matches, files, truncated) =
                self.client.grep(&path, &args.text, args.ignore_case.unwrap_or(false), args.max.unwrap_or(50).clamp(1, proto::MAX_GREP)).await?;
            let mut text = String::new();
            for m in &matches {
                if is_dir {
                    writeln!(text, "{}:{}: {}", m.path, m.line, m.text)?;
                } else {
                    writeln!(text, "{}: {}", m.line, m.text)?;
                }
            }
            write!(text, "[{} matching lines in {files} file{}{}]", matches.len(), if files == 1 { "" } else { "s" }, if truncated { "; stopped early: narrow the search" } else { "" })?;
            anyhow::Ok(text)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Check the daemon on the Switch is reachable, and optionally register this AI session's model name and supported input modalities (image, video, audio; text is always baseline). Reports round-trip time, console uptime, daemon version, whether it runs as the sysmodule or the test NRO, and the registered model/modalities.")]
    async fn ping(&self, Parameters(args): Parameters<PingArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let key = self.session_key(&context);
            let app = context
                .peer
                .peer_info()
                .map(|info| info.client_info.name.clone())
                .or_else(|| self.client.app())
                .unwrap_or_else(|| key.clone());
            let registered = if args.model.is_some() || args.modalities.is_some() {
                Some(self.models.register_ping(&key, &args, &app)?)
            } else {
                self.models.get(&key)
            };
            let info = self.client.hello().await?;
            let (rtt, uptime) = self.client.ping().await?;
            let mut out = format!(
                "daemon {} ({:?}, protocol {}) at {}: rtt {:.1} ms, console uptime {}s",
                info.version,
                info.kind,
                info.protocol,
                self.client.addr(),
                rtt.as_secs_f64() * 1000.0,
                uptime.as_secs()
            );
            if let Some(m) = registered {
                write!(out, "; model {} (modalities: {})", m.id, m.modalities)?;
            }
            anyhow::Ok(out)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Resource limit usage of the daemon's group: memory, threads, events, transfer memories, sessions (current, peak, limit), plus the daemon's own memory. From the sysmodule this is the limit all system processes share, so it shows how close the system is to running out.")]
    async fn resources(&self) -> CallToolResult {
        let result = async { anyhow::Ok(crate::client::describe_resources(&self.client.resources().await?)) };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Press buttons on the Switch through a virtual Pro Controller that the daemon attaches for the call and detaches after (about a second to settle, then the presses). Use it to get past the lock screen (b three times; after an agent reboot to an entry the sysmodule does this itself; b does nothing on the home menu, unlike a, which would launch the selected title) or to drive menus and games. then: \"still\" returns the settled screen in the same call (menus), or check with screenshot.")]
    async fn press(&self, Parameters(args): Parameters<PressArgs>) -> CallToolResult {
        let result = async {
            let buttons = crate::client::parse_buttons(&args.buttons)?;
            let times = args.times.unwrap_or(1);
            let then = args.then.as_deref().map(parse_until).transpose()?;
            self.client.press(buttons, times, args.hold_ms.unwrap_or(100)).await?;
            let done = format!("pressed {} x{times}", args.buttons);
            match then {
                Some(until) => Ok(self.look_after(&done, until, args.width).await),
                None => Ok(vec![ContentBlock::text(done)]),
            }
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Play timed input on one virtual controller, for what press can't do: analog sticks (moving in games), touch screen taps and drags, holding a button through other presses or a load, long holds, quick combos. Each step holds exactly what it lists for its time; everything is released at the end. then: \"still\" or \"change\" returns the screen in the same call; otherwise look with screenshot.")]
    async fn input(&self, Parameters(args): Parameters<InputArgs>) -> CallToolResult {
        let result = async {
            let steps = crate::client::parse_steps(&args.steps)?;
            let n = steps.len();
            let then = args.then.as_deref().map(parse_until).transpose()?;
            self.client.input(steps).await?;
            let done = format!("played {n} steps");
            match then {
                Some(until) => Ok(self.look_after(&done, until, args.width).await),
                None => Ok(vec![ContentBlock::text(done)]),
            }
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Input that runs until something happens on screen, in one call instead of a press-and-look loop: hold buttons or a stick, press a button repeatedly, or replay steps, while this PC watches the screen a few times a second and stops it the moment the condition holds. Conditions: the screen (or a region) changes, stops changing, or a piece of an earlier screenshot appears or disappears (e.g. mash A until the 'Continue' prompt appears; hold right until the door is gone; press down until the cursor reaches an item). You choose the region from a screenshot (grid: true labels coordinates); this only compares pixels, so pick something that looks the same each time it shows. Everything is released at the end. Returns what stopped it, how long it took, and the screen then (as a new shot). Reacts within about a quarter second: fine for menus, dialogue and walking, not for frame-precise play.")]
    async fn input_until(&self, Parameters(args): Parameters<InputUntilArgs>) -> CallToolResult {
        let result = async {
            use crate::until::{Action, Until};
            let action = match (&args.hold, &args.repeat, &args.steps) {
                (Some(hold), None, None) => {
                    let mut steps = crate::client::parse_steps(&format!("{}:1", hold.trim()))?;
                    anyhow::ensure!(steps.len() == 1, "hold is one thing held, like \"right\" or \"l=1,0+b\"; use steps for sequences");
                    Action::Hold(steps.remove(0))
                }
                (None, Some(buttons), None) => Action::Repeat {
                    buttons: crate::client::parse_buttons(buttons)?,
                    every: std::time::Duration::from_millis(u64::from(args.every_ms.unwrap_or(400).max(150))),
                    hold_ms: 80,
                },
                (None, None, Some(steps)) => Action::Steps(crate::client::parse_steps(steps)?),
                _ => anyhow::bail!("give exactly one of hold, repeat or steps"),
            };
            let region = args.region.as_deref().map(parse_region).transpose()?;
            let mut patch_note = None;
            let until = match args.until.as_str() {
                "change" | "changed" => Until::Change(region),
                "still" | "stable" | "settle" => Until::Still(region),
                kind @ ("appears" | "appear" | "gone" | "disappears") => {
                    let region = region.context("appears and gone need region: the part of a screenshot to look for")?;
                    let (id, jpeg) = self.shot(args.shot)?;
                    let patch = crate::until::Patch::cut(&jpeg, region)?;
                    let search = match args.search.as_deref() {
                        None => crate::until::around(region),
                        Some("screen" | "anywhere") => (0, 0, crate::capture::SCREEN.0, crate::capture::SCREEN.1),
                        Some(r) => parse_region(r)?,
                    };
                    let cut = tokio::task::spawn_blocking(move || crate::capture::reframe(&jpeg, Some(region), None)).await??;
                    patch_note = Some((id, cut));
                    if kind.starts_with("appear") { Until::Appears { patch, search } } else { Until::Gone { patch, search } }
                }
                other => anyhow::bail!("until is change, still, appears or gone, not {other:?}"),
            };
            let timeout = args.timeout_seconds.unwrap_or(15.0).clamp(0.0, crate::until::MAX_SECONDS);
            let what = args.until.clone();
            let outcome = crate::until::run(self.client.clone(), action, until, std::time::Duration::from_secs_f64(timeout)).await?;
            let id = self.remember(&outcome.jpeg);
            let mut text = if outcome.met {
                format!("{what}: stopped after {:.1} s ({} frames)", outcome.elapsed.as_secs_f64(), outcome.frames)
            } else {
                format!("timed out after {:.1} s ({} frames) without {what}", outcome.elapsed.as_secs_f64(), outcome.frames)
            };
            if let Some(((x, y), score)) = outcome.best {
                let verdict = if score < crate::until::MATCH { "a match" } else { "not a match" };
                write!(text, "; closest at {x},{y}, score {score:.2}: {verdict} (0 is identical, under {} a match, about 1 nothing like it)", crate::until::MATCH)?;
            }
            write!(text, ". The screen now is shot {id}.")?;
            let width = args.width.unwrap_or(640);
            let jpeg = scaled(outcome.jpeg, width).await?;
            let mut content = vec![ContentBlock::text(text)];
            if let Some((shot, cut)) = patch_note {
                content.push(ContentBlock::text(format!("what it looked for, from shot {shot}:")));
                content.push(jpeg_block(&cut));
            }
            content.push(jpeg_block(&jpeg));
            anyhow::Ok(content)
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Where the console is, in one call: what runs (the application: a game, or hbloader with hbmenu or an NRO with full memory; and applet mode: hbloader in the Album), whether the console is stuck on a fatal error screen (then reboot it), whether the sysmodule is still pressing B to get past the lock screen after a reboot, the keep-awake lease, uptime, whether USB is connected for fast file transfers (usb_upload), and a small screenshot of the screen (screenshot: false to skip it). Check it before pressing buttons, after a reboot, and whenever unsure what's on screen.")]
    async fn status(&self, Parameters(args): Parameters<StatusArgs>) -> CallToolResult {
        let result = async {
            let status = self.client.status().await?;
            let fatal = self.client.fatal().await?;
            let mut text = crate::client::describe_status(&status);
            if let Some(fatal) = &fatal {
                text = format!("{fatal}\n{text}");
            }
            if let Some(usb) = crate::usb::describe() {
                write!(text, "\n{usb}")?;
            }
            let others = self.client.agents().await?.map(|r| crate::client::other_agents(&r.agents, false)).unwrap_or_default();
            if !others.is_empty() {
                write!(text, "\nother agents: {} (launching, closing, input and restarts aren't coordinated between agents)", others.join("; "))?;
            }
            // Older consoles can't say.
            if let Ok(was) = self.client.playback(proto::PlaybackAction::Query).await
                && was.playing
            {
                write!(text, "\n{} (control_macro pauses or stops it)", crate::macros::control_text(&was, proto::PlaybackAction::Query))?;
            }
            let mut shot = None;
            if args.screenshot.unwrap_or(true) && fatal.is_none() {
                // The rest of the status is worth having without it.
                match self.client.screenshot().await {
                    Ok(jpeg) => shot = Some(scaled(jpeg, args.width.unwrap_or(480)).await?),
                    Err(e) => write!(text, "\nscreenshot unavailable here: {e:#}")?,
                }
            }
            let mut content = vec![ContentBlock::text(text)];
            content.extend(shot.as_deref().map(jpeg_block));
            anyhow::Ok(content)
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Quit homebrew or a game the way a person does: for the application (a game, or an NRO or hbmenu with full memory), HOME, then Close software from the home menu, so the program is asked to exit and can save and flush its output first (no error message). A hung program is ended this way too (no error). Waits up to 12 s for it to end; if it doesn't (say the home menu couldn't be reached), says so: then force_quit_app. The applet (hbmenu or an NRO in the Album) is closed with HOME. By default the applet if hbloader runs there, otherwise the application; target picks one. Only quit a game the user isn't playing.")]
    async fn quit_app(&self, Parameters(args): Parameters<QuitAppArgs>) -> CallToolResult {
        let result = async {
            let applet = match args.target.as_deref() {
                Some("applet") => true,
                Some("application") => false,
                Some(other) => anyhow::bail!("target is \"applet\" or \"application\", not {other:?}"),
                None => self.client.applet_status().await?.is_some_and(|a| a.hbl == Some(true)),
            };
            let mode = if applet { crate::launch::Mode::Applet } else { crate::launch::Mode::Full };
            anyhow::Ok(match crate::launch::close_running(&self.client, mode, crate::launch::How::Quit).await? {
                Some((app, how)) => format!("closed {} ({})", crate::client::describe_app(&app), crate::launch::describe_closed(how)),
                None if applet => "the Album wasn't open".to_string(),
                None => "no application was running".to_string(),
            })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Force-quit the application (a game, or an NRO or hbmenu with full memory): terminate it at once, whatever state it's in, like kill -9. It can't save or flush anything, and the home menu shows an error that the software closed. Only for when quit_app didn't work (it says so): quit_app ends hung programs too, without the error. Never for the applet (killing the Album crashes the home menu): quit_app closes it.")]
    async fn force_quit_app(&self) -> CallToolResult {
        let result = async {
            anyhow::Ok(match crate::launch::close_running(&self.client, crate::launch::Mode::Full, crate::launch::How::Force).await? {
                Some((app, how)) => format!("closed {} ({})", crate::client::describe_app(&app), crate::launch::describe_closed(how)),
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

    #[tool(description = "Run homebrew: start an NRO (local_path from this PC, or sd_path already on the SD card) with args from hbmenu, and return its stdout so far. mode \"full\" (default) runs it with the console's full memory in a game's place (see open_hbmenu), closing whatever application runs (ask before closing a game the user is playing); games always need this. mode \"applet\" runs small tools quickly from the Album, and is refused for the projects in projects.json (the user's games). An NRO already running in that mode is closed first; then hbmenu's NetLoader receives it. The NRO keeps running after this returns; read more output with nro_output, see it with screenshot, stop it with quit_app. Takes about 30 s plus the transfer.")]
    async fn run_nro(&self, Parameters(args): Parameters<RunNroArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let program = match (args.local_path, args.sd_path) {
                (Some(local), None) => {
                    let nro = tokio::fs::read(&local).await.with_context(|| format!("reading {local}"))?;
                    let name = std::path::Path::new(&local).file_name().context("local_path has no file name")?.to_string_lossy().into_owned();
                    let dest = check_path(&args.dest.unwrap_or_else(|| format!("/switch/kickstand/{name}")), true)?;
                    self.permit(&context.peer, &dest, Need::Write, false, &format!("put {name} at {dest} and run it")).await?;
                    crate::launch::Program::Local { nro, dest }
                }
                (None, Some(sd)) => {
                    let sd = check_path(&sd, false)?;
                    self.permit(&context.peer, &sd, Need::Read, false, &format!("run {sd}")).await?;
                    crate::launch::Program::Sd(sd)
                }
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
            let nro_args = args.args.unwrap_or_default();
            self.readable_outputs(&nro_args);
            let started = crate::launch::run_nro(&self.client, program, nro_args, mode).await?;
            let mut text = started.notes.join("\n");
            if let Some(stream) = started.output {
                let handle = crate::launch::OutputLog::collect(&self.output, stream, symbols, started.pid);
                if let Some(old) = self.collector.lock().unwrap().replace(handle) {
                    old.abort();
                }
                let wait = args.wait_seconds.unwrap_or(5).min(60);
                text.push_str(&self.read_output(wait, &OutputView::default()).await);
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
                content.push(jpeg_block(&jpg));
            }
            anyhow::Ok(CallToolResult::success(content))
        };
        result.await.unwrap_or_else(fail)
    }

    #[tool(description = "Build, deploy, run and watch a project in one call (projects are set up in ~/.config/kickstand/projects.json; list them with the projects tool): runs its build command on this PC (a failure returns the end of the build log), checks the NRO was made from the ELF, sends it to its place on the SD card and runs it with full memory, then watches it like supervise. Returns the build result, what happened (exited, crashed with a symbolized report, ended, fatal, looks hung with backtraces, or still running), the output's last lines and a screenshot when there is one. Closes whatever runs first: don't use it while the user is playing. Its output stays readable with nro_output, and memory tools use its ELF afterwards.")]
    async fn test_build(&self, Parameters(args): Parameters<TestBuildArgs>) -> CallToolResult {
        let result = async {
            let project = crate::project::get(&args.project)?;
            let settings = watch_settings(args.seconds, args.quiet_seconds, true);
            self.readable_outputs(args.args.as_ref().unwrap_or(&project.args));
            let run = crate::project::test(&self.client, &project, args.skip_build.unwrap_or(false), args.args, &self.output, &settings).await?;
            if let Some(handle) = run.collector
                && let Some(old) = self.collector.lock().unwrap().replace(handle)
            {
                old.abort();
            }
            *self.elf.lock().unwrap() = Some((run.elf.0, Some(run.elf.1)));
            let mut content = vec![ContentBlock::text(run.text)];
            if let Some(jpeg) = run.screenshot {
                content.push(jpeg_block(&jpeg));
            }
            anyhow::Ok(content)
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "The projects test_build knows (from ~/.config/kickstand/projects.json): name, folder, build command, NRO, where it goes on the SD card, and the arguments it runs with (test_build's args replace them for one run). A project's args field holds a launch command line you'd otherwise retype; ask the user to add a project rather than editing the file yourself.")]
    async fn projects(&self) -> CallToolResult {
        let result = async {
            let mut out = String::new();
            for (name, p) in crate::project::load_all()? {
                writeln!(out, "{name}: {} (build: {}; {} -> {})", p.dir.display(), p.build.as_deref().unwrap_or("none"), p.nro.display(), p.dest)?;
                if !p.args.is_empty() {
                    writeln!(out, "    args: {}", p.args.join(" "))?;
                }
            }
            anyhow::Ok(if out.is_empty() { "no projects".to_string() } else { out })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Watch the running homebrew (after run_nro) until something happens, and say what: EXITED (with its last output), CRASHED (a new crash report for it, symbolized, with Atmosphère's screenshot), FATAL (the console stopped; rebooted into hekate entry 1 unless reboot_on_fatal is false), LOOKS HUNG (no output for quiet_seconds and a screen frozen for 5 s more: every thread's backtrace and a screenshot), or STILL RUNNING when seconds run out. Use it after starting a build to test, instead of polling status.")]
    async fn supervise(&self, Parameters(args): Parameters<SuperviseArgs>) -> CallToolResult {
        let result = async {
            let elf = self.elf(args.elf, None)?;
            let settings = watch_settings(args.seconds, args.quiet_seconds, args.reboot_on_fatal.unwrap_or(true));
            let output = self.output.lock().unwrap().is_open().then_some(&self.output);
            let report = crate::watchdog::supervise(&self.client, output, &elf, &settings).await?;
            let mut content = vec![ContentBlock::text(report.text)];
            if let Some(jpeg) = report.screenshot {
                content.push(jpeg_block(&jpeg));
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

    #[tool(description = "More stdout from the NRO run_nro last started: what arrived since the last read, and whether it's still running (its stdout open). On long or chatty runs, save tokens with grep (only matching lines, plus after lines after each), max_lines (only the newest) and timestamps (when each line arrived, to time phases). Lines left out are still marked read.")]
    async fn nro_output(&self, Parameters(args): Parameters<NroOutputArgs>) -> CallToolResult {
        let result = async {
            let grep = args.grep.as_deref().map(regex::Regex::new).transpose().context("grep isn't a valid regular expression")?;
            let view = OutputView { grep, after: args.after.unwrap_or(0).min(100), max_lines: args.max_lines, timestamps: args.timestamps.unwrap_or(false) };
            let wait = args.wait_seconds.unwrap_or(0).min(60);
            anyhow::Ok(self.read_output(wait, &view).await.trim_start().to_string())
        };
        result.await.map_or_else(fail, ok)
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
            let id = self.remember(&jpeg);
            let mut text = format!("shot {id}: {} bytes", jpeg.len());
            if let Some(path) = &args.local_path {
                tokio::fs::write(path, &jpeg).await?;
                write!(text, ", saved to {path}")?;
            }
            let grid = args.grid.unwrap_or(false);
            let jpeg = tokio::task::spawn_blocking(move || crate::capture::frame(&jpeg, region, args.width, grid)).await??;
            anyhow::Ok(vec![ContentBlock::text(text), jpeg_block(&jpeg)])
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Wait for the Switch's screen to change (a load to finish, a menu or dialog to appear, the game to react to input) or to stop changing (a transition, fade or scrolling text to end), then look at it: one call instead of polling with screenshots. Returns the screen when that happens, or when the timeout passes (and says which).")]
    async fn wait_screen(&self, Parameters(args): Parameters<WaitScreenArgs>) -> CallToolResult {
        let result = async {
            let until = parse_until(args.until.as_deref().unwrap_or("change"))?;
            let region = args.region.as_deref().map(parse_region).transpose()?;
            let timeout = args.timeout_seconds.unwrap_or(10.0).clamp(0.0, crate::capture::MAX_WAIT_SECONDS);
            self.look("", until, region, std::time::Duration::from_secs_f64(timeout), args.width.unwrap_or(640)).await
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Record the Switch's screen for a few seconds (default 3, max 30), e.g. to inspect an animation or a bug, saved on this PC (by default in ~/Pictures/kickstand/). When the running game records gameplay (or homebrew runs in such a game's place) and ffmpeg is installed, this streams the console's own 30 fps H.264 video and saves it as an .mp4 (no re-encoding), plus a GIF; otherwise it falls back to repeated screenshots (a few frames a second) and says why. Adapts to the modalities registered in ping: video-capable sessions also get the clip, image-capable sessions a contact sheet of up to 6 evenly spaced frames, text-only sessions a frame/motion summary.")]
    async fn record_gif(&self, Parameters(args): Parameters<RecordGifArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let seconds = args.seconds.unwrap_or(3.0);
            let width = args.width.unwrap_or(640).clamp(64, 1280);
            let fps = args.fps.unwrap_or(5).clamp(1, crate::capture::MAX_FPS);
            let path = match args.local_path {
                Some(p) => std::path::PathBuf::from(p),
                None => default_capture_path("gif")?,
            };
            // The console's own video first; screenshots when it can't.
            let mut mp4 = None;
            let source;
            let shots = match self.gameplay_video(seconds, &path).await {
                Ok((saved, clip_fps, frames, kb)) => {
                    let decode = saved.clone();
                    let shots =
                        tokio::task::spawn_blocking(move || crate::video::decode_shots(&decode, clip_fps, fps, width)).await??;
                    source = format!("the console's gameplay video: {frames} frames at {clip_fps:.0} fps, {kb} KB, saved as {}", saved.display());
                    mp4 = Some(saved);
                    shots
                }
                Err(why) => {
                    source = format!("screenshots (not the console's video stream: {why:#})");
                    crate::capture::record(&self.client, seconds, fps).await?
                }
            };
            if shots.is_empty() {
                anyhow::bail!("no frames came out of {source}");
            }
            {
                let mut ring = self.video_ring.lock().unwrap();
                for s in &shots {
                    ring.push_jpeg(s.at, s.jpeg.clone());
                }
            }
            let span = shots.last().map_or(0.0, |s| s.at.as_secs_f64());
            let frames = shots.len();
            let pref = self.session_modalities(&context).preferred_visual();
            let shots_for_gif = shots.clone();
            let (gif, sheet) = tokio::task::spawn_blocking(move || {
                anyhow::Ok((crate::capture::gif(&shots_for_gif, width)?, crate::capture::contact_sheet(&shots_for_gif, 6)?))
            })
            .await??;
            tokio::fs::write(&path, &gif).await?;
            let gif_fps = if span > 0.0 { (frames - 1) as f64 / span } else { 0.0 };
            let head = format!("from {source}\nsaved {} ({frames} frames, {gif_fps:.1} fps, {} KB)", path.display(), gif.len() / 1024);
            match pref {
                VisualFormat::Video => {
                    let (clip_path, bytes, mime) = match &mp4 {
                        Some(p) if p.extension().is_some_and(|e| e == "mp4") => (p.clone(), tokio::fs::read(p).await?, "video/mp4"),
                        _ => {
                            let clip_path = path.with_extension("mp4");
                            let clip = tokio::task::spawn_blocking(move || {
                                crate::capture::export_clip(&shots, width, VisualFormat::Video, Some(clip_path))
                            })
                            .await??;
                            (clip.path.unwrap_or_else(|| path.clone()), clip.bytes, clip.mime_type)
                        }
                    };
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    anyhow::Ok(vec![
                        ContentBlock::text(format!("{head}; video modality clip at {}", clip_path.display())),
                        ContentBlock::image(b64, mime),
                        jpeg_block(&sheet),
                    ])
                }
                VisualFormat::ContactSheet => anyhow::Ok(vec![ContentBlock::text(format!("{head}; contact sheet below")), jpeg_block(&sheet)]),
                VisualFormat::TextSummary => {
                    let clip = tokio::task::spawn_blocking(move || {
                        crate::capture::export_clip(&shots, width, VisualFormat::TextSummary, None)
                    })
                    .await??;
                    anyhow::Ok(vec![ContentBlock::text(format!("{head}; {}", clip.summary))])
                }
            }
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "List a directory on the Switch's SD card. Keys, NAND backups and /Nintendo are not readable. Outside Kickstand's folders, crash reports and the projects' folders, the user is asked first (a question in this app) before list_dir, stat, read_file, tail_file or download can look; if they say no, don't look for another way.")]
    async fn list_dir(&self, Parameters(args): Parameters<PathArg>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            self.permit(&context.peer, &path, Need::Read, true, &format!("see what's in {path}")).await?;
            let (mut entries, truncated) = self.client.list_dir(&path).await?;
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
            let mut out = format!("{path} ({} entries)\n", entries.len());
            for e in &entries {
                if e.is_dir {
                    writeln!(out, "  {}/", e.name)?;
                } else {
                    writeln!(out, "  {}  {} bytes", e.name, e.size)?;
                }
            }
            if truncated {
                writeln!(out, "  ... truncated at {} entries", proto::MAX_DIR_ENTRIES)?;
            }
            anyhow::Ok(out)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Size and type of a file or directory on the Switch's SD card.")]
    async fn stat(&self, Parameters(args): Parameters<PathArg>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            self.permit(&context.peer, &path, Need::Read, false, &format!("look at {path}")).await?;
            let e = self.client.stat(&path).await?;
            anyhow::Ok(if e.is_dir { format!("{path}: directory") } else { format!("{path}: file, {} bytes", e.size) })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "A log file on the Switch's SD card as it grows: its last lines, then (with follow_seconds) what's added, checked every half second; until stops at the first new line containing some text. Returns next, the offset to pass as from to carry on where this left off. For homebrew that logs to a file, or that wasn't started by run_nro (whose stdout nro_output has).")]
    async fn tail_file(&self, Parameters(args): Parameters<TailArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            self.permit(&context.peer, &path, Need::Read, false, &format!("read {path}")).await?;
            let (mut text, mut next) = match args.from {
                Some(from) => (String::new(), from),
                None => crate::tail::last_lines(&self.client, &path, args.lines.unwrap_or(40) as usize).await?,
            };
            let follow = args.follow_seconds.unwrap_or(0.0).clamp(0.0, 60.0);
            let mut notes = Vec::new();
            if follow > 0.0 || (args.until.is_some() && args.from.is_some()) {
                let length = std::time::Duration::from_secs_f64(if follow > 0.0 { follow } else { 15.0 });
                let found = crate::tail::follow(&self.client, &path, next, length, args.until.as_deref(), READ_DEFAULT as usize).await?;
                if found.restarted {
                    notes.push("the file got shorter (truncated or replaced), so this reads it again from the start".to_string());
                }
                if found.dropped {
                    notes.push(format!("only the last {} KiB of what was added", READ_DEFAULT / 1024));
                }
                match &args.until {
                    Some(until) if found.seen => notes.push(format!("stopped at a line containing {until:?}")),
                    Some(until) => notes.push(format!("no new line contained {until:?} within {:.0} s", length.as_secs_f64())),
                    None => {}
                }
                text.push_str(&found.text);
                next = found.next;
            }
            let mut out = format!("{path}: next {next}");
            for note in notes {
                write!(out, "; {note}")?;
            }
            out.push('\n');
            out.push_str(if text.is_empty() { "(nothing new)" } else { &text });
            anyhow::Ok(out)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Read part of a file on the Switch's SD card, such as a log or a crash report in /atmosphere/crash_reports. Text comes back as text; binary data as base64.")]
    async fn read_file(&self, Parameters(args): Parameters<ReadArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            self.permit(&context.peer, &path, Need::Read, false, &format!("read {path}")).await?;
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
    async fn download(&self, Parameters(args): Parameters<DownloadArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            self.permit(&context.peer, &path, Need::Read, false, &format!("copy {path} to this PC")).await?;
            let size = self.client.stat(&path).await?.size;
            let (data, _) = self.client.read(&path, 0, size).await?;
            tokio::fs::write(&args.local_path, &data).await?;
            anyhow::Ok(format!("saved {path} ({} bytes) to {}", data.len(), args.local_path))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Upload a file from this PC to the Switch's SD card, e.g. a fresh .nro build, the fastest way available: straight to the SD card if it's mounted on this PC, over USB for files of 4 MB or more when a cable is plugged in, else over Wi-Fi (also when USB fails; the result says why). Streamed in chunks and verified with CRC-32 before it replaces the old file. usb_upload forces USB. Atmosphère's and hekate's folders and the top of the card are never writable. Outside Kickstand's folders and the projects' folders the user is asked first (a question in this app); if they say no, don't look for another way.")]
    async fn upload(&self, Parameters(args): Parameters<UploadArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let remote = check_path(&args.remote_path, true)?;
            self.permit(&context.peer, &remote, Need::Write, false, &format!("write {remote}")).await?;
            let local = std::path::Path::new(&args.local_path);
            let sent = crate::transfer::put_file(&self.client, local, &remote, crate::transfer::Transport::Auto).await?;
            anyhow::Ok(format!("uploaded {} -> {remote}: {}", args.local_path, sent.describe()))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Upload a file from this PC to the Switch's SD card over a USB cable, streamed from disk. Preferred over upload for large files (ROMs, ISOs, .o2r archives) whenever status reports a USB cable. Goes through Kickstand's own USB interface, the same CRC-checked upload as over Wi-Fi: nothing on the console is opened or closed and a running game keeps running (other USB homebrew like haze can't use the port meanwhile). If the SD card is mounted on this PC instead, copies straight to it. If it fails asking for a one-time setup on this PC (a udev rule on Linux, a driver on Windows), tell the user what it says; don't try to do it. Same path and permission rules as upload.")]
    async fn usb_upload(&self, Parameters(args): Parameters<UploadArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let remote = check_path(&args.remote_path, true)?;
            self.permit(&context.peer, &remote, Need::Write, false, &format!("write {remote}")).await?;
            let stats = crate::usb::upload(&self.client, std::path::Path::new(&args.local_path), &remote).await?;
            let secs = stats.elapsed.as_secs_f64();
            let out = format!(
                "uploaded {} -> {remote} via {}: {} bytes in {secs:.1}s ({:.2} MB/s)",
                args.local_path,
                stats.via,
                stats.size,
                stats.size as f64 / 1e6 / secs.max(1e-6)
            );
            anyhow::Ok(out)
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
                writeln!(out, "  {}. {e}", i + 1)?;
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

    #[tool(description = "Hot-reload a homebrew NRO without a full Home Menu round-trip: optionally builds a project, uploads the NRO (preferring USB when connected), then presses + so the running NRO exits inside the same hbloader process. An app implementing Kickstand's next-load hook chain-loads the new build itself (fastest); otherwise it goes through hbmenu's NetLoader, or a full relaunch if needed. Reports which path was taken and the time per phase (upload, exit, load), and optionally replays a saved macro to return to the exact in-game state. Much faster than run_nro for iterative edit-compile-test loops.")]
    async fn reload_nro(&self, Parameters(args): Parameters<ReloadNroArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let mut build_note = None;
            let (local_path, sd_path, dest, nro_args, elf_path, module_name, default_mode) = if let Some(proj_name) = &args.project {
                let project = crate::project::get(proj_name)?;
                if args.build.unwrap_or(false) {
                    build_note = Some(project.build().await?);
                }
                let local = args.local_path.clone().or_else(|| Some(project.dir.join(&project.nro).to_string_lossy().into_owned()));
                let dest = args.dest.clone().or_else(|| Some(project.dest.clone()));
                let nro_args = args.args.clone().unwrap_or_else(|| project.args.clone());
                let elf = args.elf.clone().or_else(|| Some(project.dir.join(&project.elf).to_string_lossy().into_owned()));
                let module = args.module.clone().or_else(|| Some(project.module()));
                (local, args.sd_path.clone(), dest, nro_args, elf, module, crate::launch::Mode::Full)
            } else {
                (
                    args.local_path.clone(),
                    args.sd_path.clone(),
                    args.dest.clone(),
                    args.args.clone().unwrap_or_default(),
                    args.elf.clone(),
                    args.module.clone(),
                    crate::launch::Mode::Full,
                )
            };

            let program = match (local_path, sd_path) {
                (Some(local), None) => {
                    let path_buf = std::path::PathBuf::from(&local);
                    let nro = tokio::fs::read(&path_buf).await.with_context(|| format!("reading {local}"))?;
                    let name = path_buf.file_name().context("local_path has no file name")?.to_string_lossy().into_owned();
                    let dest = check_path(&dest.unwrap_or_else(|| format!("/switch/kickstand/{name}")), true)?;
                    self.permit(&context.peer, &dest, Need::Write, false, &format!("put {name} at {dest} and reload it")).await?;
                    crate::launch::Program::Local { nro, dest }
                }
                (None, Some(sd)) => {
                    let sd = check_path(&sd, false)?;
                    self.permit(&context.peer, &sd, Need::Read, false, &format!("reload {sd}")).await?;
                    crate::launch::Program::Sd(sd)
                }
                _ => anyhow::bail!("give a project name, local_path, or sd_path"),
            };

            let symbols = match &elf_path {
                Some(elf) => Some(crate::symbolize::Symbolizer::new(elf.as_ref(), module_name.as_deref())?.shared()),
                None => None,
            };
            if let Some(elf) = &elf_path {
                *self.elf.lock().unwrap() = Some((elf.clone(), module_name.clone()));
            }
            let mode = crate::launch::Mode::parse(args.mode.as_deref(), default_mode)?;
            self.readable_outputs(&nro_args);
            let exit_buttons = args.exit_buttons.as_deref().map(crate::client::parse_buttons).transpose()?;

            let options = crate::reload::ReloadOptions {
                program,
                args: nro_args,
                mode,
                use_usb: args.prefer_usb.unwrap_or(true),
                exit_buttons,
                replay_macro: args.replay_macro,
                hook: args.hook,
            };
            let reloaded = crate::reload::hot_reload(&self.client, options).await?;
            let mut notes = Vec::new();
            if let Some(bn) = build_note {
                notes.push(bn);
            }
            notes.push(reloaded.summary());
            notes.extend(reloaded.started.notes);
            let mut text = notes.join("\n");
            if let Some(stream) = reloaded.started.output {
                let handle = crate::launch::OutputLog::collect(&self.output, stream, symbols, reloaded.started.pid);
                if let Some(old) = self.collector.lock().unwrap().replace(handle) {
                    old.abort();
                }
                let wait = args.wait_seconds.unwrap_or(5).min(60);
                text.push_str(&self.read_output(wait, &OutputView::default()).await);
            }
            anyhow::Ok(text)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Sample the running application's CPU call stacks (50-100 Hz) via frame-pointer unwinding, symbolize against its DWARF `.elf`, write a `.folded` stack file and interactive `.svg` flamegraph on this PC, and return a token-efficient hotspot report.")]
    async fn profile(&self, Parameters(args): Parameters<ProfileArgs>) -> CallToolResult {
        let result = async {
            let elf = self.elf(args.elf, args.module)?;
            let seconds = args.seconds.unwrap_or(3.0).clamp(0.1, 15.0);
            let duration_ms = (seconds * 1000.0).round() as u32;
            let hz = args.hz.unwrap_or(50).clamp(5, 100);
            let interval_ms = (1000 / hz).clamp(10, 200);
            let thread_id = match args.thread_id.as_deref() {
                Some(s) => {
                    let s = s.trim();
                    let val = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                        Some(hex) => u64::from_str_radix(hex, 16),
                        None => s.parse::<u64>(),
                    }
                    .with_context(|| format!("invalid thread_id {s:?}"))?;
                    Some(val)
                }
                None => None,
            };
            let out_path = match args.svg_path {
                Some(p) => Some(std::path::PathBuf::from(p)),
                None => default_capture_path("svg").ok(),
            };
            let out = crate::profile::run(
                &self.client,
                &elf,
                &crate::profile::ProfileOptions {
                    duration_ms,
                    interval_ms,
                    thread_id,
                    min_tree_pct: args.min_tree_pct.unwrap_or(crate::profile::DEFAULT_MIN_TREE_PCT),
                    max_hotspots: args.max_hotspots.unwrap_or(crate::profile::DEFAULT_MAX_HOTSPOTS),
                    out: out_path,
                },
            )
            .await?;
            anyhow::Ok(out.text)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Hear the last few seconds (up to 5) of the running game's or homebrew's sound, after the fact: Kickstand's sysmodule records it continuously in the background from the console's gameplay recorder (grc:d), so this returns what already played (just after a glitch, a missing sound effect or a crash). Saved as a .wav on this PC and analysed (level, dominant frequency, silence, clipping, pops); reference_wav says whether and where a sound effect occurs in it. Only games that allow video capture can be heard, and homebrew running in place of such a game; when nothing can be heard the reply says why and what to change (e.g. the one call that opts homebrew in). Output follows the modalities registered in ping (the .wav for audio, a spectrogram for images, else text).")]
    async fn audio_replay(&self, Parameters(args): Parameters<AudioReplayArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let seconds = args.seconds.unwrap_or(5.0).clamp(0.1, 5.0);
            let (samples, capture) = self.client.audio_replay(seconds).await?;
            let got = samples.len() as f64 / f64::from(proto::audio::BYTES_PER_SECOND / 2);
            let mut note = format!("{} (asked for {seconds:.1} s, got {got:.2} s", crate::audio::capture_line(&capture));
            if got + 0.05 < seconds {
                note.push_str(": that's all since the sound started or last resumed");
            }
            note.push(')');
            self.audio_reply(samples, note, args.local_path, args.reference_wav, &context).await
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Record the next N seconds (0.1-30, default 3) of the running game's or homebrew's sound as it plays, streamed live from the console's gameplay recorder (grc:d): start it, then make the sound happen (input, play_macro) from another call or let the game run. Saved as a .wav on this PC and analysed like audio_replay, with optional reference_wav matching. Same limits as audio_replay: only games that allow video capture, and homebrew in such a game's place; otherwise the reply says why.")]
    async fn record_audio(&self, Parameters(args): Parameters<RecordAudioArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let seconds = args.seconds.unwrap_or(3.0).clamp(0.1, 30.0);
            let rec = self.client.record_audio(seconds).await?;
            let got = rec.samples.len() as f64 / f64::from(proto::audio::BYTES_PER_SECOND / 2);
            let mut note = format!("recorded {got:.2} s of live console audio");
            if rec.gaps_ms > 0 {
                let _ = write!(note, "; {} ms of gaps filled with silence (the stream fell behind or the sound paused)", rec.gaps_ms);
            }
            if let Some(why) = &rec.stopped {
                let _ = write!(note, "; stopped early: {why}");
            }
            self.audio_reply(rec.samples, note, args.local_path, args.reference_wav, &context).await
        };
        result.await.map_or_else(fail, CallToolResult::success)
    }

    #[tool(description = "Query or set the Switch's system audio output volume (0-100%) and mute state. Call with no arguments to see the output target (speaker, headphones, TV, USB, Bluetooth), volume, mute, and whether Kickstand is hearing the running application (see audio_replay).")]
    async fn audio_control(&self, Parameters(args): Parameters<AudioControlArgs>) -> CallToolResult {
        let result = async {
            let status = self.client.audio_control(args.volume, args.muted).await?;
            let mut text = format!(
                "audio output: {}, volume {}%{}",
                status.target,
                status.volume_pct,
                if status.muted { " (muted)" } else { "" },
            );
            match self.client.audio_capture().await {
                Ok(c) => match crate::audio::why_silent(&c) {
                    None => {
                        let _ = write!(text, "\n{}", crate::audio::capture_line(&c));
                    }
                    Some(why) => {
                        let _ = write!(text, "\nnot hearing the application: {why}");
                    }
                },
                Err(e) => {
                    let _ = write!(text, "\n{e:#}");
                }
            }
            anyhow::Ok(text)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Delete a file or directory on the Switch's SD card, e.g. old builds or test output. Only inside the writable folders (see upload); directories must be empty unless recursive is true.")]
    async fn delete(&self, Parameters(args): Parameters<DeleteArgs>, context: RequestContext<RoleServer>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, true)?;
            let is_dir = self.client.stat(&path).await.is_ok_and(|e| e.is_dir);
            self.permit(&context.peer, &path, Need::Write, is_dir, &format!("delete {path}{}", if is_dir { " and everything in it" } else { "" })).await?;
            let was_dir = self.client.delete(&path, args.recursive.unwrap_or(false)).await?;
            anyhow::Ok(format!("deleted {path}{}", if was_dir { " (directory)" } else { "" }))
        };
        result.await.map_or_else(fail, ok)
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "kickstand",
    instructions = "Tools for a real Nintendo Switch running the kickstand daemon. Paths are absolute from the SD card root. Start with ping. The console stays awake for 30 minutes after each tool call and can't be woken remotely once asleep: call keep_awake before going longer than that without a tool call, and keep_awake with minutes 0 when finished. To run homebrew, use run_nro (it opens hbmenu itself) and read its stdout with nro_output; status says where the console is (what runs, the lock screen after a reboot, keep-awake, other agents using it) with a small screenshot. When another agent is changing things on the console, tools that press buttons, launch, close or restart start their result with a warning: don't fight over the console; wait or ask the user."
)]
impl ServerHandler for Tools {
    /// Runs the tool, warning first when another agent changed something on the console lately and the
    /// tool could clash with it.
    async fn call_tool(&self, request: CallToolRequestParams, context: RequestContext<RoleServer>) -> Result<CallToolResponse, rmcp::ErrorData> {
        if self.client.app().is_none() {
            // What the app calls itself: in the handshake, or (stateless MCP) with each request.
            let name = context.peer.peer_info().map(|info| info.client_info.name.clone()).or_else(|| context.meta.client_info().map(|info| info.name));
            if let Some(name) = name {
                self.client.set_app(&name).await;
            }
        }
        let warning = if CONTENTIOUS.contains(&request.name.as_ref()) { self.clash().await } else { None };
        let mut response = self.tool_router.call(ToolCallContext::new(self, request, context)).await?;
        if let (Some(warning), CallToolResponse::Complete(result)) = (warning, &mut response) {
            result.content.insert(0, ContentBlock::text(warning));
        }
        Ok(response)
    }
}

/// What allowing `scope` covers, for an access question.
fn covers(scope: &str, file: bool, need: Need) -> String {
    let what = if file { access::describe(scope, true) } else { format!("everything in {}", access::describe(scope, false)) };
    format!("{what}{}", if need == Need::Write { " (reading, writing and deleting)" } else { " (reading only)" })
}

/// Tools that change what the console does, so can clash with another agent using it.
const CONTENTIOUS: &[&str] = &[
    "press",
    "input",
    "input_until",
    "play_macro",
    "control_macro",
    "type_text",
    "quit_app",
    "force_quit_app",
    "launch_title",
    "open_hbmenu",
    "run_nro",
    "reload_nro",
    "test_build",
    "supervise",
    "write_memory",
    "usb_upload",
    "reboot",
    "sysmodule",
    "keep_awake",
    "audio_control",
];

impl Tools {
    /// A warning if other agents changed something on the console in the last two minutes.
    async fn clash(&self) -> Option<String> {
        let others = crate::client::other_agents(&self.client.agents().await.ok()??.agents, true);
        (!others.is_empty()).then(|| {
            format!(
                "Heads up, the console is shared right now: {}. Input, launching, closing and restarts aren't coordinated between agents, so this may clash with what they're doing (check the screen). If it matters, wait for them to finish or ask the user.",
                others.join("; ")
            )
        })
    }
}

/// Every tool's name.
#[cfg(test)]
pub fn tool_names() -> Vec<String> {
    Tools::tool_router().list_all().into_iter().map(|t| t.name.into()).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn contentious_tools_exist() {
        let names = super::tool_names();
        for name in super::CONTENTIOUS {
            assert!(names.iter().any(|n| n == name), "no tool named {name}");
        }
    }
}

#[cfg(test)]
mod output_tests {
    use super::OutputView;

    fn lines(text: &[&str]) -> Vec<(f64, String)> {
        text.iter().enumerate().map(|(i, l)| (i as f64, l.to_string())).collect()
    }

    #[test]
    fn grep_keeps_matches_and_the_lines_after() {
        let view = OutputView { grep: Some(regex::Regex::new("^report=").unwrap()), after: 2, ..Default::default() };
        let all = lines(&["boot", "report=1", "a", "b", "c", "report=2", "d"]);
        let (shown, notes) = view.pick(&all);
        assert_eq!(shown, [1, 2, 3, 5, 6]);
        assert!(notes[0].starts_with("5 of 7 new lines"), "{notes:?}");
    }

    #[test]
    fn max_lines_keeps_the_newest() {
        let view = OutputView { max_lines: Some(2), ..Default::default() };
        let (shown, notes) = view.pick(&lines(&["a", "b", "c", "d"]));
        assert_eq!(shown, [2, 3]);
        assert_eq!(notes, ["the 2 before these left out (max_lines 2)"]);
    }
}
