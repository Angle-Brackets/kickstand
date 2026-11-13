// SPDX-License-Identifier: GPL-3.0-only
//! Hot-reloading a homebrew NRO inside the hbloader process that runs it, without a Home Menu round trip.
//!
//! An NRO running under nx-hbloader that returns from `main` doesn't end the process: hbloader loads the next
//! NRO, which is hbmenu unless the NRO called `envSetNextLoad`. [`hot_reload`] uses that, trying in order:
//! 1. **Upload** the new build to its place on the SD card while the old one still runs (USB when asked
//!    for and available, else Wi-Fi).
//! 2. **The app's next-load hook** ([`ReloadPath::Hook`], opt-in by the app): write [`NEXT_LOAD`] and press
//!    `+`; an app that implements the hook claims the file on its way out (reads it, deletes it, and only if
//!    the delete worked calls `envSetNextLoad` with its two lines), and hbloader loads the new build
//!    straight away. No hbmenu, no NetLoader, no Kickstand NRO.
//! 3. **hbmenu** ([`ReloadPath::Hbmenu`]): `+` exits the NRO to hbmenu in the same process; Y starts
//!    NetLoader, which is sent the Kickstand NRO with `--open <dest>`, which chain-loads the build.
//! 4. **A full relaunch** ([`ReloadPath::Cold`], [`launch::run_nro`]) when hbloader isn't running or the NRO
//!    didn't come back to hbmenu.
//!
//! Then optionally a saved macro is replayed to get back to the state under test.
//!
//! ## The next-load contract (TOOLS.md has C and Rust snippets)
//! [`NEXT_LOAD`] is two lines of text: the NRO path for `envSetNextLoad` (`sdmc:/switch/x/x.nro`), then its
//! argv string, passed to `envSetNextLoad` verbatim. hbloader hands that string to the next NRO as is, so it
//! follows hbmenu's convention: every argument in double quotes, the program path first and nxlink's
//! `<hex addr>_NXLINK_` marker last, so the new build's stdout comes back to this PC. Why it can't misfire:
//! - **The delete is the commit.** The app chain-loads only if *its* delete of the file worked; Kickstand
//!   withdraws an unclaimed request by deleting it. Exactly one of the two deletes succeeds, so Kickstand
//!   always knows which way it went (file gone = claimed), and there is no moment where both act.
//! - **PC launches only.** Apps honour the file only when they were started over nxlink (their argv has the
//!   `_NXLINK_` marker; libnx's `__nxlink_host` is set), never when someone opens them from hbmenu, so a
//!   request left behind by an interrupted reload can't hijack a manual launch.
//! - **Cleared before other launches.** [`launch::run_nro`], and a reload that doesn't use the hook, delete a
//!   leftover request before the next NRO could see it.
//!
//! Support is detected, not configured: an app that implements the hook names the file, so its NRO contains
//! the bytes `kickstand/next-load` ([`implements_hook`]); a build already on the SD card is tried when the
//! hook worked for that path earlier in this process. [`ReloadOptions::hook`] overrides both.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use proto::Button;
use proto::path::NEXT_LOAD;

use crate::client::{Client, check_path};
use crate::launch::{self, Mode, Program, Started};
use crate::nxlink;
use crate::transfer::{self, Transport};

/// How long the running NRO gets to tear down after the exit button(s) before checking hbloader is still there.
const EXIT_SETTLE: Duration = if cfg!(test) { Duration::from_millis(30) } else { Duration::from_millis(250) };

/// How long to wait for hbmenu's NetLoader in the in-process path before falling back to a full relaunch
/// (which costs ~14 s, so a slow hbmenu start is worth waiting out).
const HOT_NETLOADER_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(5) };

/// How often Y is pressed again in the in-process path: hbmenu loses a press made while it's still
/// starting, and ignores one while NetLoader is already active.
const HOT_Y_REPEAT: Duration = Duration::from_millis(1000);

/// How long to wait for the chain-loaded NRO to connect its `nxlink` stdout.
const HOT_OUTPUT_WAIT: Duration = if cfg!(test) { Duration::from_millis(500) } else { launch::OUTPUT_WAIT };

/// How long the app gets to claim the next-load request after the exit button(s). Generous, because only
/// apps that name the request file are asked, and a large one can take seconds to tear down before it gets
/// to the hook (Uprising NX: about 5 s); a late claim is still safe, since only one delete can win.
const HOOK_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(15) };
/// How often to look whether the request was claimed.
const HOOK_POLL: Duration = if cfg!(test) { Duration::from_millis(10) } else { Duration::from_millis(40) };

/// Bytes every NRO implementing the hook contains: it has to name the request file.
const HOOK_MARKER: &[u8] = b"kickstand/next-load";
/// hbloader's buffers for `envSetNextLoad` (nx-hbloader's `g_nextNroPath` and `g_nextArgv`), NUL included.
const NEXT_PATH_MAX: usize = 512;
const NEXT_ARGV_MAX: usize = 2048;

/// SD card NROs whose app claimed a next-load request in this process: their next reload tries the hook too.
static HOOKED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
/// The Kickstand NRO's bytes for the hbmenu path, by file size, so a reload doesn't read ~700 KB over Wi-Fi
/// each time. (A same-sized replacement would be missed; any Kickstand NRO can open an NRO, so that's harmless.)
static LAUNCHER_CACHE: Mutex<Option<(u64, Vec<u8>)>> = Mutex::new(None);

#[derive(Debug)]
pub struct ReloadOptions {
    pub program: launch::Program,
    pub args: Vec<String>,
    pub mode: launch::Mode,
    pub use_usb: bool,
    pub exit_buttons: Option<Vec<proto::Button>>,
    pub replay_macro: Option<String>,
    /// Try the app's next-load hook: `None` detects support (see the module docs), `Some` forces it on or off.
    pub hook: Option<bool>,
}

/// Which way the new build was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadPath {
    /// The running app chain-loaded it itself (the next-load hook).
    Hook,
    /// The app exited to hbmenu in the same process; NetLoader and the Kickstand NRO started it.
    Hbmenu,
    /// hbmenu was reopened from the Home Menu ([`launch::run_nro`]).
    Cold,
}

impl ReloadPath {
    pub fn name(self) -> &'static str {
        match self {
            ReloadPath::Hook => "hook",
            ReloadPath::Hbmenu => "hbmenu",
            ReloadPath::Cold => "cold",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            ReloadPath::Hook => "through the app's next-load hook",
            ReloadPath::Hbmenu => "in-process through hbmenu",
            ReloadPath::Cold => "with a full relaunch",
        }
    }
}

/// Where the time went.
#[derive(Clone, Copy, Debug, Default)]
pub struct Phases {
    /// Putting the build on the SD card (or checking it's there).
    pub upload: Duration,
    /// From pressing the exit button(s) until the old NRO was out of the way: the request claimed (hook), or
    /// NetLoader listening (hbmenu). Includes the press itself, about 1.3 s: Kickstand's virtual controller
    /// is attached and given a second to be noticed. `None` for a full relaunch.
    pub exit: Option<Duration>,
    /// From then until the new build connected its stdout (`None` if it never did); for a full relaunch,
    /// all of [`launch::run_nro`].
    pub load: Option<Duration>,
}

#[derive(Debug)]
pub struct Reloaded {
    pub started: launch::Started,
    pub path: ReloadPath,
    pub phases: Phases,
    pub elapsed: Duration,
    #[allow(dead_code)]
    pub transfer_note: String,
}

impl Reloaded {
    /// One line: the path taken and the time per phase.
    pub fn summary(&self) -> String {
        let secs = |d: Duration| format!("{:.2} s", d.as_secs_f64());
        let mut parts = vec![format!("upload {}", secs(self.phases.upload))];
        if self.path == ReloadPath::Cold {
            parts.extend(self.phases.load.map(|d| format!("relaunch {}", secs(d))));
        } else if self.path == ReloadPath::Hook {
            // The app exits and hbloader loads the new build while the button is still held, so the two
            // can't be told apart from here.
            let exit = self.phases.exit.unwrap_or_default();
            parts.push(match self.phases.load {
                Some(load) => format!("press, exit and load {}", secs(exit + load)),
                None => format!("press and exit {}, load ? (no stdout)", secs(exit)),
            });
        } else {
            parts.extend(self.phases.exit.map(|d| format!("exit {}", secs(d))));
            parts.push(match self.phases.load {
                Some(d) => format!("load {}", secs(d)),
                None => "load ? (no stdout)".into(),
            });
        }
        format!("reloaded {} in {:.1} s ({})", self.path.describe(), self.elapsed.as_secs_f64(), parts.join(", "))
    }

    /// The path and phases (seconds) for `--json` output.
    pub fn json(&self) -> serde_json::Value {
        let secs = |d: Option<Duration>| d.map(|d| (d.as_secs_f64() * 1000.0).round() / 1000.0);
        serde_json::json!({
            "path": self.path.name(),
            "elapsed_s": secs(Some(self.elapsed)),
            "upload_s": secs(Some(self.phases.upload)),
            "exit_s": secs(self.phases.exit),
            "load_s": secs(self.phases.load),
        })
    }
}

/// Builds the `--open <dest> [args...]` argument list for the Kickstand NRO launcher.
pub fn chainload_args(sd_dest: &str, args: &[String]) -> Vec<String> {
    ["--open".to_string(), sd_dest.to_string()].into_iter().chain(args.iter().cloned()).collect()
}

/// Resolves the buttons used to ask a running NRO to exit back to `hbmenu` (`Plus` by default).
pub fn default_exit_buttons(custom: Option<&[Button]>) -> Vec<Button> {
    match custom {
        Some(buttons) => buttons.to_vec(),
        None => vec![Button::Plus],
    }
}

/// Whether an NRO implements the next-load hook: it names the request file.
pub fn implements_hook(nro: &[u8]) -> bool {
    memchr::memmem::find(nro, HOOK_MARKER).is_some()
}

/// The contents of [`NEXT_LOAD`] for starting `sd_dest` with `args`: its `sdmc:` path, then the argv string
/// (hbmenu's convention: all quoted, the path first, `host_arg` last). libnx splits argv on whitespace
/// outside double quotes, with no escapes, and drops empty arguments, so an argument can't be empty or
/// contain `"` (nor a line break, for the file).
pub fn next_load_request(sd_dest: &str, args: &[String], host_arg: Option<&str>) -> Result<String> {
    let path = format!("sdmc:{sd_dest}");
    if let Some(bad) = args.iter().find(|a| a.is_empty() || a.contains(['"', '\n', '\r', '\0'])) {
        bail!("argument {bad:?} can't go through hbloader's argv (it's empty, or has a double quote or a line break)");
    }
    let argv = std::iter::once(path.as_str())
        .chain(args.iter().map(String::as_str))
        .chain(host_arg)
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(" ");
    if path.len() >= NEXT_PATH_MAX {
        bail!("{path} is longer than hbloader's {NEXT_PATH_MAX}-byte path buffer");
    }
    if argv.len() >= NEXT_ARGV_MAX {
        bail!("the arguments are longer than hbloader's {NEXT_ARGV_MAX}-byte argv buffer");
    }
    Ok(format!("{path}\n{argv}\n"))
}

/// nxlink's `<hex addr>_NXLINK_` marker for this PC's address as the console sees it, so a chain-loaded
/// NRO's stdout comes here. (A connected UDP socket picks the route without sending anything.)
async fn nxlink_host_arg(console: &str) -> Option<String> {
    let sock = tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await.ok()?;
    sock.connect((console, nxlink::SERVER_PORT)).await.ok()?;
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(host_marker(ip)),
        _ => None,
    }
}

/// libnx reads the marker's 8 hex digits as `in_addr.s_addr`, which holds the octets in memory order.
fn host_marker(ip: std::net::Ipv4Addr) -> String {
    format!("{:08x}_NXLINK_", u32::from_le_bytes(ip.octets()))
}

fn is_not_found(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("NotFound")
}

/// Hot-reloads an NRO in-process when `hbloader` is running, falling back to [`launch::run_nro`] when needed.
pub async fn hot_reload(client: &Client, opts: ReloadOptions) -> Result<Reloaded> {
    let sd_dest = validate_destination(client, &opts.program, opts.mode).await?;
    if let Some(macro_name) = &opts.replay_macro {
        // Validate the macro up front before spending time transferring or reloading the NRO.
        crate::macros::load(macro_name)?;
    }
    let want_hook = opts.hook.unwrap_or_else(|| match &opts.program {
        Program::Local { nro, .. } => implements_hook(nro),
        Program::Sd(_) => HOOKED.lock().unwrap().contains(&sd_dest),
    });

    let start = Instant::now();
    let transfer_note = stage_nro_on_sd(client, &opts.program, &sd_dest, opts.use_usb).await?;
    let mut phases = Phases { upload: start.elapsed(), ..Phases::default() };

    let attempt =
        try_in_process_reload(client, &sd_dest, &opts.args, opts.mode, opts.exit_buttons.as_deref(), want_hook).await?;
    let (mut started, path) = match attempt {
        HotOutcome::Hot { mut started, path, exit, load } => {
            started.notes.insert(0, transfer_note.clone());
            phases.exit = exit;
            phases.load = load;
            (started, path)
        }
        HotOutcome::Cold { reason, notes } => {
            // If the Kickstand launcher NRO is on the SD card, use `Program::Sd` so `run_nro` doesn't
            // re-upload the NRO binary a second time over slow NetLoader TCP.
            let fallback_program = if client.stat(launch::LAUNCHER).await.is_ok() {
                Program::Sd(sd_dest.clone())
            } else {
                opts.program
            };
            let relaunch = Instant::now();
            let mut started = launch::run_nro(client, fallback_program, opts.args, opts.mode).await?;
            phases.load = Some(relaunch.elapsed());
            let mut first = vec![transfer_note.clone()];
            first.extend(notes);
            first.push(format!("in-process reload unavailable ({reason}); fell back to full relaunch"));
            started.notes.splice(0..0, first);
            (started, ReloadPath::Cold)
        }
    };

    if let Some(macro_name) = &opts.replay_macro {
        let replayed = crate::macros::play(client, macro_name).await?;
        started.notes.push(format!("replayed macro: {}", replayed.summary()));
    }

    Ok(Reloaded { started, path, phases, elapsed: start.elapsed(), transfer_note })
}

/// Checks path rules, `.nro` extension, `/switch/` placement for local uploads, and the applet-mode game rule.
async fn validate_destination(client: &Client, program: &Program, mode: Mode) -> Result<String> {
    let raw_target = match program {
        Program::Local { dest, .. } => dest,
        Program::Sd(path) => path,
    };
    if mode == Mode::Applet && launch::is_game(raw_target, &crate::project::load_all().unwrap_or_default()) {
        bail!("{raw_target} is a game: it runs with full memory only (mode \"full\"), never in applet mode");
    }
    match program {
        Program::Local { dest, .. } => {
            let dest = check_path(dest, true)?;
            if !dest.starts_with("/switch/") {
                bail!("NRO destination must be under /switch/ (got {dest})");
            }
            if !dest.to_ascii_lowercase().ends_with(".nro") {
                bail!("{dest} isn't an .nro path");
            }
            Ok(dest)
        }
        Program::Sd(path) => {
            let path = check_path(path, false)?;
            if !path.to_ascii_lowercase().ends_with(".nro") || client.stat(&path).await.is_err() {
                bail!("{path} isn't an NRO on the SD card");
            }
            Ok(path)
        }
    }
}

/// Uploads a local NRO to `sd_dest` (over USB if requested and available, otherwise over Wi-Fi) while the
/// current NRO is still running, or stats an existing SD card NRO.
async fn stage_nro_on_sd(client: &Client, program: &Program, sd_dest: &str, use_usb: bool) -> Result<String> {
    match program {
        Program::Local { nro, .. } => {
            // USB first when a cable is in and the build is big enough to repay it, else Wi-Fi.
            let transport = if use_usb { Transport::Auto } else { Transport::Wifi };
            let sent = transfer::put_bytes(client, nro, sd_dest, transport).await.with_context(|| format!("uploading {sd_dest}"))?;
            Ok(format!("uploaded {sd_dest}: {}", sent.describe()))
        }
        Program::Sd(_) => {
            let entry = client.stat(sd_dest).await.with_context(|| format!("checking {sd_dest} on the SD card"))?;
            Ok(format!("using {sd_dest} already on the SD card ({} bytes)", entry.size))
        }
    }
}

/// The Kickstand NRO, from [`LAUNCHER_CACHE`] when its size hasn't changed.
async fn launcher_bytes(client: &Client) -> Result<Vec<u8>> {
    let size = client.stat(launch::LAUNCHER).await?.size;
    if let Some((cached_size, bytes)) = &*LAUNCHER_CACHE.lock().unwrap()
        && *cached_size == size
    {
        return Ok(bytes.clone());
    }
    let (bytes, _) = client.read(launch::LAUNCHER, 0, u64::MAX).await?;
    *LAUNCHER_CACHE.lock().unwrap() = Some((size, bytes.clone()));
    Ok(bytes)
}

enum HotOutcome {
    Hot { started: Started, path: ReloadPath, exit: Option<Duration>, load: Option<Duration> },
    Cold { reason: String, notes: Vec<String> },
}

enum HookOutcome {
    /// The app claimed the request; hbloader is loading the new build.
    Taken { started: Started, exit: Duration, load: Option<Duration> },
    /// Kickstand withdrew the request: the app didn't claim it after the exit button(s), pressed at `pressed_at`.
    Withdrawn { pressed_at: Instant, note: String },
    /// The request couldn't be made (nothing was written or pressed).
    Skipped { note: String },
}

fn button_names(buttons: &[Button]) -> String {
    buttons.iter().map(|b| b.name()).collect::<Vec<_>>().join("+")
}

/// Attempts the fast in-process reload when `hbloader` is already running in `mode`: the app's next-load
/// hook first when `want_hook`, then hbmenu's NetLoader.
async fn try_in_process_reload(
    client: &Client,
    sd_dest: &str,
    args: &[String],
    mode: Mode,
    exit_buttons: Option<&[Button]>,
    want_hook: bool,
) -> Result<HotOutcome> {
    let Some(app) = launch::status(client, mode).await? else {
        return Ok(HotOutcome::Cold { reason: "no application was running".into(), notes: Vec::new() });
    };
    if app.hbl != Some(true) {
        return Ok(HotOutcome::Cold {
            reason: format!("title {:016x} isn't running under hbloader", app.program_id),
            notes: Vec::new(),
        });
    }

    let mut notes = Vec::new();
    let buttons = default_exit_buttons(exit_buttons);
    let already_at_hbmenu = app.pid != 0 && app.pid == launch::HBMENU_PID.load(Ordering::Relaxed) && exit_buttons.is_none();
    // When the exit button(s) went in: the start of the exit phase.
    let mut pressed_at = None;

    if want_hook && !already_at_hbmenu && !buttons.is_empty() {
        match try_hook(client, sd_dest, args, &buttons, app.pid).await? {
            HookOutcome::Taken { started, exit, load } => {
                return Ok(HotOutcome::Hot { started, path: ReloadPath::Hook, exit: Some(exit), load });
            }
            HookOutcome::Withdrawn { pressed_at: at, note } => {
                notes.push(note);
                pressed_at = Some(at);
            }
            HookOutcome::Skipped { note } => notes.push(note),
        }
    } else {
        // A request left by an interrupted reload must not redirect this exit.
        let _ = client.delete(NEXT_LOAD, false).await;
    }

    let launcher = match launcher_bytes(client).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return Ok(HotOutcome::Cold { reason: format!("couldn't read {} ({e})", launch::LAUNCHER), notes });
        }
    };

    if pressed_at.is_none() && !already_at_hbmenu && !buttons.is_empty() {
        pressed_at = Some(Instant::now());
        client.press(buttons.clone(), 1, 120).await?;
        notes.push(format!(
            "pressed {} to exit the running NRO back to hbmenu in-process (pid {})",
            button_names(&buttons),
            app.pid
        ));
        tokio::time::sleep(EXIT_SETTLE).await;
    } else if already_at_hbmenu {
        notes.push(format!("hbmenu was already open in hbloader (pid {})", app.pid));
    }
    if pressed_at.is_some() {
        match launch::status(client, mode).await? {
            Some(now) if now.hbl == Some(true) => {}
            _ => {
                return Ok(HotOutcome::Cold {
                    reason: "hbloader exited to the Home Menu instead of returning to hbmenu".into(),
                    notes,
                });
            }
        }
    }
    let exit_from = pressed_at.unwrap_or_else(Instant::now);

    let mut sock = launch::start_netloader_every(client, HOT_NETLOADER_WAIT, HOT_Y_REPEAT).await.ok();
    if sock.is_none() && already_at_hbmenu && !buttons.is_empty() {
        // HBMENU_PID was stale and an NRO was actually running; press the exit button(s) and try once more.
        client.press(buttons.clone(), 1, 120).await?;
        notes.push(format!(
            "hbmenu didn't answer right away; pressed {} to exit the running NRO to hbmenu",
            button_names(&buttons)
        ));
        tokio::time::sleep(EXIT_SETTLE).await;
        sock = launch::start_netloader_every(client, HOT_NETLOADER_WAIT, HOT_Y_REPEAT).await.ok();
    }

    let Some(sock) = sock else {
        return Ok(HotOutcome::Cold {
            reason: format!(
                "hbmenu's NetLoader didn't answer within {:.1} s after in-process exit",
                HOT_NETLOADER_WAIT.as_secs_f64()
            ),
            notes,
        });
    };
    let exit = exit_from.elapsed();

    let output = nxlink::OutputListener::bind().await?;
    launch::nro_starting();
    let load_from = Instant::now();
    let launcher_dest = launch::LAUNCHER.trim_start_matches("/switch/").to_string();
    nxlink::send(sock, &launcher, &launcher_dest, &chainload_args(sd_dest, args)).await?;
    notes.push(format!(
        "chainloaded {sd_dest} in-process through {} (same hbloader pid {})",
        launch::LAUNCHER,
        app.pid
    ));

    let pid = launch::status(client, mode).await?.map_or(app.pid, |a| a.pid);
    let output = output.accept(HOT_OUTPUT_WAIT).await?;
    let load = output.as_ref().map(|_| load_from.elapsed());
    if output.is_none() {
        notes.push("the NRO didn't connect its stdout (it may not use nxlink stdio)".into());
    }

    Ok(HotOutcome::Hot { started: Started { notes, pid, output }, path: ReloadPath::Hbmenu, exit: Some(exit), load })
}

/// Whether the next-load request is still there. Looks at the folder rather than the file: a stat opens the
/// file, and the app's delete could fail while it's open.
async fn request_pending(client: &Client) -> Result<bool> {
    let (dir, file) = NEXT_LOAD.rsplit_once('/').unwrap_or(("", NEXT_LOAD));
    let (entries, _) = client.list_dir(dir).await?;
    Ok(entries.iter().any(|e| e.name.eq_ignore_ascii_case(file)))
}

/// Deletes an unclaimed next-load request: true if this delete removed it (the app can no longer take it),
/// false if it was already gone (the app claimed it). The app deletes the file to claim it, so whichever
/// delete works decides. Retried briefly, since the app may have the file open while it reads it.
async fn withdraw(client: &Client) -> Result<bool> {
    let mut tries = 0;
    loop {
        match client.delete(NEXT_LOAD, false).await {
            Ok(_) => return Ok(true),
            Err(e) if is_not_found(&e) => return Ok(false),
            Err(_) if tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.context("withdrawing the next-load request")),
        }
    }
}

/// Writes the next-load request, presses the exit button(s) and waits for the app to claim the request,
/// withdrawing it if the app doesn't.
async fn try_hook(client: &Client, sd_dest: &str, args: &[String], buttons: &[Button], pid: u64) -> Result<HookOutcome> {
    let host = nxlink_host_arg(client.host()).await;
    let request = match next_load_request(sd_dest, args, host.as_deref()) {
        Ok(request) => request,
        Err(e) => return Ok(HookOutcome::Skipped { note: format!("not using the app's next-load hook: {e:#}") }),
    };
    // Listening before the request exists, so the new build finds this PC however fast it starts.
    let output = nxlink::OutputListener::bind().await?;
    client.upload(NEXT_LOAD, request.as_bytes()).await.context("writing the next-load request")?;
    // The old build's stdout closes as it exits; that won't mean hbmenu is showing.
    launch::nro_starting();
    // Before the press: the app exits while it's held, and the press itself takes over a second.
    let pressed_at = Instant::now();
    if let Err(e) = client.press(buttons.to_vec(), 1, 120).await {
        let _ = withdraw(client).await;
        return Err(e);
    }
    let names = button_names(buttons);

    let claimed = loop {
        match request_pending(client).await {
            Ok(false) => break true,
            Err(e) => {
                let _ = withdraw(client).await;
                return Err(e.context("waiting for the app to claim the next-load request"));
            }
            Ok(true) if pressed_at.elapsed() >= HOOK_WAIT => break !withdraw(client).await?,
            Ok(true) => tokio::time::sleep(HOOK_POLL).await,
        }
    };
    if !claimed {
        HOOKED.lock().unwrap().remove(sd_dest);
        return Ok(HookOutcome::Withdrawn {
            pressed_at,
            note: format!(
                "pressed {names}; the app didn't claim the next-load request within {:.1} s (it doesn't implement \
                 the hook, wasn't started from a PC, or didn't exit), so it was withdrawn",
                HOOK_WAIT.as_secs_f64()
            ),
        });
    }
    let exit = pressed_at.elapsed();
    HOOKED.lock().unwrap().insert(sd_dest.to_string());

    let load_from = Instant::now();
    let output = output.accept(HOT_OUTPUT_WAIT).await?;
    let load = output.as_ref().map(|_| load_from.elapsed());
    let mut notes = vec![format!(
        "pressed {names}; the app claimed the next-load request {:.2} s after the press began and hbloader \
         loaded {sd_dest} directly (same process, pid {pid}; no hbmenu)",
        exit.as_secs_f64()
    )];
    if output.is_none() {
        notes.push(format!(
            "the NRO didn't connect its stdout within {:.0} s (it may not use nxlink stdio)",
            HOT_OUTPUT_WAIT.as_secs_f64()
        ));
    }
    Ok(HookOutcome::Taken { started: Started { notes, pid, output }, exit, load })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::mock;

    // The tests that listen on nxlink's fixed ports take turns.
    use crate::launch::TEST_STARTS as PORTS;

    fn options(program: Program, args: Vec<String>, hook: Option<bool>) -> ReloadOptions {
        ReloadOptions { program, args, mode: Mode::Full, use_usb: false, exit_buttons: None, replay_macro: None, hook }
    }

    /// A mock console with `app` running under hbloader and the Kickstand NRO on its SD card.
    async fn console_with_app(name: &str, hook: bool) -> (std::path::PathBuf, Arc<mock::Console>, Client) {
        let root = std::env::temp_dir().join(format!("kickstand-reload-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("switch/kickstand")).unwrap();
        std::fs::create_dir_all(root.join("kickstand")).unwrap();
        std::fs::write(root.join("switch/kickstand/kickstand.nro"), b"KICKSTAND-LAUNCHER-NRO").unwrap();
        let console = Arc::new(mock::Console::new(root.clone(), vec!["tok".into()], None));
        *console.app.lock().unwrap() = mock::MockApp {
            running: Some(proto::App { pid: 0x42, program_id: 0x0100_0000_0000_1000, hbl: Some(true) }),
            next_load_hook: hook,
            ..Default::default()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(mock::serve_console(listener, console.clone()));
        (root, console, Client::new(addr, "tok".into()))
    }

    /// A fake hbmenu NetLoader: returns the file name, the NRO and the argv it was sent.
    fn fake_netloader(netloader: tokio::net::TcpListener) -> tokio::task::JoinHandle<(String, Vec<u8>, Vec<u8>)> {
        tokio::spawn(async move {
            let (mut s, _) = netloader.accept().await.unwrap();
            let name_len = s.read_u32_le().await.unwrap() as usize;
            let mut name = vec![0; name_len];
            s.read_exact(&mut name).await.unwrap();
            let size = s.read_u32_le().await.unwrap() as usize;
            s.write_i32_le(0).await.unwrap();
            let mut inflate = flate2::Decompress::new(true);
            let mut out = Vec::with_capacity(size);
            while out.len() < size {
                let len = s.read_u32_le().await.unwrap() as usize;
                let mut piece = vec![0; len];
                s.read_exact(&mut piece).await.unwrap();
                inflate.decompress_vec(&piece, &mut out, flate2::FlushDecompress::None).unwrap();
            }
            s.write_i32_le(0).await.unwrap();
            let argv_len = s.read_u32_le().await.unwrap() as usize;
            let mut argv = vec![0; argv_len];
            s.read_exact(&mut argv).await.unwrap();
            (String::from_utf8(name).unwrap(), out, argv)
        })
    }

    #[test]
    fn builds_chainload_args_and_default_exit_buttons() {
        assert_eq!(
            chainload_args("/switch/mygame/mygame.nro", &["--level".into(), "3".into()]),
            vec!["--open", "/switch/mygame/mygame.nro", "--level", "3"]
        );
        assert_eq!(default_exit_buttons(None), vec![Button::Plus]);
        assert_eq!(
            default_exit_buttons(Some(&[Button::Minus, Button::Plus])),
            vec![Button::Minus, Button::Plus]
        );
        assert!(default_exit_buttons(Some(&[])).is_empty());
    }

    #[test]
    fn next_load_requests_follow_hbmenus_argv_convention() {
        let marker = host_marker("192.168.86.96".parse().unwrap());
        // The example from hbmenu's quoting (see the nxlink crate): 192.168.86.96 is 6056a8c0.
        assert_eq!(marker, "6056a8c0_NXLINK_");
        assert_eq!(
            next_load_request("/switch/mygame/mygame.nro", &["--level".into(), "a b".into()], Some(&marker)).unwrap(),
            "sdmc:/switch/mygame/mygame.nro\n\"sdmc:/switch/mygame/mygame.nro\" \"--level\" \"a b\" \"6056a8c0_NXLINK_\"\n"
        );
        assert_eq!(next_load_request("/switch/x.nro", &[], None).unwrap(), "sdmc:/switch/x.nro\n\"sdmc:/switch/x.nro\"\n");
        for bad in ["say \"hi\"", "two\nlines", ""] {
            let err = next_load_request("/switch/x.nro", &[bad.into()], None).unwrap_err().to_string();
            assert!(err.contains("argv"), "{err}");
        }
        let long = "x".repeat(NEXT_ARGV_MAX);
        assert!(next_load_request("/switch/x.nro", &[long], None).is_err());
    }

    #[test]
    fn detects_the_hook_by_the_file_name_in_the_nro() {
        let mut nro = vec![0u8; 4096];
        assert!(!implements_hook(&nro));
        nro[1000..1025].copy_from_slice(b"sdmc:/kickstand/next-load");
        assert!(implements_hook(&nro));
    }

    #[tokio::test]
    async fn refuses_destinations_outside_switch_or_non_nro() {
        let (root, _console, client) = console_with_app("val", false).await;
        let local = |dest: &str| Program::Local { nro: b"NRO0".to_vec(), dest: dest.into() };
        let bad_dir = hot_reload(&client, options(local("/kickstand/test.nro"), Vec::new(), None)).await.unwrap_err().to_string();
        assert!(bad_dir.contains("/switch/"), "{bad_dir}");
        let bad_ext = hot_reload(&client, options(local("/switch/mygame/mygame.bin"), Vec::new(), None)).await.unwrap_err().to_string();
        assert!(bad_ext.contains(".nro"), "{bad_ext}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn stages_nro_over_wifi_and_mounted_sd() {
        let root = std::env::temp_dir().join(format!("kickstand-reload-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("atmosphere")).unwrap();
        std::fs::create_dir_all(root.join("switch")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(mock::serve_console(listener, Arc::new(mock::Console::new(root.clone(), vec!["tok".into()], None))));
        let client = Client::new(addr, "tok".into());

        // 1. Wi-Fi upload (`use_usb: false`)
        let payload = b"NRO0-wifi-payload".to_vec();
        let note = stage_nro_on_sd(
            &client,
            &Program::Local { nro: payload.clone(), dest: "/switch/mygame/mygame.nro".into() },
            "/switch/mygame/mygame.nro",
            false,
        )
        .await
        .unwrap();
        assert!(note.contains("via Wi-Fi"), "{note}");
        assert_eq!(std::fs::read(root.join("switch/mygame/mygame.nro")).unwrap(), payload);

        // 2. USB requested when no USB cable/mount exists -> seamlessly falls back to Wi-Fi
        let payload2 = b"NRO0-usb-fallback-payload".to_vec();
        let note2 = stage_nro_on_sd(
            &client,
            &Program::Local { nro: payload2.clone(), dest: "/switch/mygame/mygame.nro".into() },
            "/switch/mygame/mygame.nro",
            true,
        )
        .await
        .unwrap();
        assert!(note2.contains("USB unavailable") || note2.contains("via"), "{note2}");
        assert_eq!(std::fs::read(root.join("switch/mygame/mygame.nro")).unwrap(), payload2);

        // 3. Existing SD card NRO (`Program::Sd`)
        let note3 = stage_nro_on_sd(&client, &Program::Sd("/switch/mygame/mygame.nro".into()), "/switch/mygame/mygame.nro", false)
            .await
            .unwrap();
        assert!(note3.contains("already on the SD card"), "{note3}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn hot_reloads_in_process_via_launcher_when_netloader_answers() {
        let _ports = PORTS.lock().await;
        // Bind NetLoader port 28280 on 127.0.0.1 if free; skip only if another process holds 28280.
        let Ok(netloader) = tokio::net::TcpListener::bind(("127.0.0.1", nxlink::SERVER_PORT)).await else {
            return;
        };
        let (root, _console, client) = console_with_app("hot", false).await;
        // Left by an interrupted reload: this exit must not see it.
        std::fs::write(root.join("kickstand/next-load"), "sdmc:/switch/old.nro\n\"sdmc:/switch/old.nro\"\n").unwrap();
        let netloader_task = fake_netloader(netloader);

        let game_nro = b"NEW-GAME-BUILD-NRO".to_vec();
        let program = Program::Local { nro: game_nro.clone(), dest: "/switch/mygame/mygame.nro".into() };
        let reloaded = hot_reload(&client, options(program, vec!["--fast".into()], None)).await.unwrap();

        assert_eq!(reloaded.path, ReloadPath::Hbmenu, "notes: {:?}", reloaded.started.notes);
        assert_eq!(reloaded.started.pid, 0x42);
        assert!(reloaded.elapsed < Duration::from_secs(5));
        assert!(reloaded.transfer_note.contains("/switch/mygame/mygame.nro"), "{}", reloaded.transfer_note);
        assert!(reloaded.phases.exit.is_some());
        assert!(reloaded.summary().contains("in-process through hbmenu"), "{}", reloaded.summary());
        assert_eq!(std::fs::read(root.join("switch/mygame/mygame.nro")).unwrap(), game_nro);
        assert!(!root.join("kickstand/next-load").exists(), "the leftover request was cleared");

        let (sent_name, sent_nro, sent_argv) = netloader_task.await.unwrap();
        assert_eq!(sent_name, "kickstand/kickstand.nro");
        assert_eq!(sent_nro, b"KICKSTAND-LAUNCHER-NRO");
        assert_eq!(sent_argv, b"--open\0/switch/mygame/mygame.nro\0--fast\0");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn an_app_with_the_hook_chainloads_the_new_build_itself() {
        let _ports = PORTS.lock().await;
        let (root, console, client) = console_with_app("hook", true).await;
        // The new build connects its stdout once the app has chain-loaded it.
        let app = console.clone();
        let new_build = tokio::spawn(async move {
            loop {
                if app.app.lock().unwrap().chainloaded.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let mut out = tokio::net::TcpStream::connect(("127.0.0.1", nxlink::CLIENT_PORT)).await.unwrap();
            out.write_all(b"new build up\n").await.unwrap();
            out
        });

        // It names the request file, so the hook is tried without being asked for.
        let game_nro = b"NRO0 ... sdmc:/kickstand/next-load ...".to_vec();
        let program = Program::Local { nro: game_nro.clone(), dest: "/switch/mygame/mygame.nro".into() };
        let reloaded = hot_reload(&client, options(program, vec!["--level".into(), "3".into()], None)).await.unwrap();
        let _out = new_build.await.unwrap();

        assert_eq!(reloaded.path, ReloadPath::Hook, "notes: {:?}", reloaded.started.notes);
        assert_eq!(reloaded.started.pid, 0x42);
        assert!(reloaded.started.output.is_some());
        assert!(reloaded.phases.exit.is_some() && reloaded.phases.load.is_some(), "{:?}", reloaded.phases);
        assert!(reloaded.summary().contains("next-load hook"), "{}", reloaded.summary());
        assert_eq!(reloaded.json()["path"], "hook");
        let (path, argv) = console.app.lock().unwrap().chainloaded.clone().unwrap();
        assert_eq!(path, "sdmc:/switch/mygame/mygame.nro");
        assert!(
            argv.starts_with("\"sdmc:/switch/mygame/mygame.nro\" \"--level\" \"3\" \"0100007f_NXLINK_\""),
            "{argv}"
        );
        assert!(!root.join("kickstand/next-load").exists());
        assert_eq!(std::fs::read(root.join("switch/mygame/mygame.nro")).unwrap(), game_nro);

        // The same NRO from the SD card next time: the hook worked for it, so it's tried again.
        console.app.lock().unwrap().chainloaded = None;
        let app = console.clone();
        let again = tokio::spawn(async move {
            while app.app.lock().unwrap().chainloaded.is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::net::TcpStream::connect(("127.0.0.1", nxlink::CLIENT_PORT)).await.unwrap()
        });
        let reloaded = hot_reload(&client, options(Program::Sd("/switch/mygame/mygame.nro".into()), Vec::new(), None)).await.unwrap();
        let _out = again.await.unwrap();
        assert_eq!(reloaded.path, ReloadPath::Hook, "notes: {:?}", reloaded.started.notes);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn an_unclaimed_request_is_withdrawn_and_hbmenu_takes_over() {
        let _ports = PORTS.lock().await;
        let Ok(netloader) = tokio::net::TcpListener::bind(("127.0.0.1", nxlink::SERVER_PORT)).await else {
            return;
        };
        // The running build doesn't implement the hook (an older build than the one being sent).
        let (root, console, client) = console_with_app("withdrawn", false).await;
        let netloader_task = fake_netloader(netloader);

        let program = Program::Local { nro: b"NEW sdmc:/kickstand/next-load".to_vec(), dest: "/switch/mygame/mygame.nro".into() };
        let reloaded = hot_reload(&client, options(program, Vec::new(), None)).await.unwrap();

        assert_eq!(reloaded.path, ReloadPath::Hbmenu, "notes: {:?}", reloaded.started.notes);
        assert!(reloaded.started.notes.iter().any(|n| n.contains("withdrawn")), "{:?}", reloaded.started.notes);
        // + was pressed once, for the hook; the hbmenu path didn't press it again.
        assert!(!reloaded.started.notes.iter().any(|n| n.contains("back to hbmenu in-process")), "{:?}", reloaded.started.notes);
        assert!(reloaded.phases.exit.unwrap() >= HOOK_WAIT);
        assert!(!root.join("kickstand/next-load").exists(), "the request was withdrawn");
        assert!(console.app.lock().unwrap().chainloaded.is_none());
        let (sent_name, _, sent_argv) = netloader_task.await.unwrap();
        assert_eq!(sent_name, "kickstand/kickstand.nro");
        assert_eq!(sent_argv, b"--open\0/switch/mygame/mygame.nro\0");

        // hook: false never writes a request, even for an app with the hook.
        console.app.lock().unwrap().next_load_hook = true;
        let netloader = tokio::net::TcpListener::bind(("127.0.0.1", nxlink::SERVER_PORT)).await.unwrap();
        let netloader_task = fake_netloader(netloader);
        let program = Program::Local { nro: b"NEW sdmc:/kickstand/next-load".to_vec(), dest: "/switch/mygame/mygame.nro".into() };
        let reloaded = hot_reload(&client, options(program, Vec::new(), Some(false))).await.unwrap();
        assert_eq!(reloaded.path, ReloadPath::Hbmenu);
        assert!(console.app.lock().unwrap().chainloaded.is_none());
        netloader_task.await.unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }
}
