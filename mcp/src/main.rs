// SPDX-License-Identifier: GPL-3.0-only
//! kickstand: an MCP server (stdio) for the daemon on the Switch, a desktop app for setting it up and
//! following along, and a few CLI commands for using the daemon by hand.

mod access;
mod audio;
mod capture;
mod client;
mod config;
mod crash;
mod desktop;
mod install;
mod launch;
mod link;
mod macros;
mod memory;
mod modalities;
mod mock;
mod nxlink;
mod out;
mod pairing;
mod profile;
mod project;
mod questions;
mod reload;
mod symbolize;
mod tail;
mod tools;
mod until;
mod update;
mod usb;
mod video;
mod watch;
mod watchdog;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use serde_json::json;

use client::{Client, check_path};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// The Switch's IP address. Defaults to $SWITCH_IP, then ~/.config/kickstand/host.
    #[arg(long, global = true)]
    host: Option<String>,
    /// The daemon's port.
    #[arg(long, global = true, default_value_t = proto::DEFAULT_PORT)]
    port: u16,
    /// Print the result as one JSON object ({"ok": true, ...}, or {"ok": false, "error": ...} with exit
    /// code 1) for scripts; `run` and `tail -f` print one object per line as things happen.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server on stdio (what AI apps start; the default when stdin is a pipe).
    Serve {
        /// The AI app this server is for, so the desktop app can tell whose agent is whose: `link` writes it
        /// (code, agy, ...); any other name works too. Without it, the name the app gives in MCP's handshake.
        #[arg(long)]
        app: Option<String>,
    },
    /// Open the desktop app (the default otherwise): find the Switch, pair, link an AI app, and see what
    /// the agent is doing.
    Desktop,
    /// Add the desktop app to this computer's app menu, with its icon (Linux: ~/.local/share/applications;
    /// macOS: ~/Applications). It runs this copy of Kickstand.
    Install {
        /// Take it out instead.
        #[arg(long)]
        remove: bool,
    },
    /// Create the shared token if there isn't one, and print it and where the console needs it.
    Token,
    /// Add Kickstand to an AI app: code (Claude Code), desktop (Claude Desktop), gemini (Gemini CLI), agy
    /// (Antigravity CLI), antigravity, codex or lmstudio. Without one, show where it's linked.
    Link {
        app: Option<String>,
        /// Take it out instead.
        #[arg(long)]
        remove: bool,
    },
    /// List the Switches running Kickstand on this network.
    Discover {
        /// Probe this address instead of broadcasting.
        #[arg(long)]
        to: Option<String>,
    },
    /// Pair with the Switch showing CODE (open Kickstand from hbmenu on it to see the code), and use it from
    /// now on. Finds it on the network unless --host is given.
    Pair { code: String },
    /// Update Kickstand: this PC's copy to the latest release, then the Switch to the PC's version (its
    /// background service restarts, about 15 s; only while nothing is running on it).
    Update {
        /// Only say whether there's something to update.
        #[arg(long)]
        check: bool,
        /// Only update the Switch (to this PC's version).
        #[arg(long)]
        switch: bool,
    },
    /// Make the Switch forget this PC: agents here can't use it until you pair again with the code on its
    /// screen. Other PCs paired with it keep working.
    Unpair,
    /// Check the daemon answers.
    Ping,
    /// List a directory on the SD card.
    Ls { path: String },
    /// Print a file from the SD card.
    Cat { path: String },
    /// Copy a file from the SD card to this PC.
    Get { path: String, local: PathBuf },
    /// Upload a file to the SD card (over Wi-Fi, or --usb over USB).
    Put {
        local: PathBuf,
        path: String,
        /// Copy over a USB cable (Kickstand's own USB interface, or a mounted SD card) instead of Wi-Fi.
        #[arg(long)]
        usb: bool,
    },
    /// Upload a file to the SD card over a USB cable, for large files like ROMs or .o2r archives: through
    /// Kickstand's own USB interface (whatever runs on the console keeps running), or straight to its SD card
    /// when that's mounted on this PC.
    UsbPut { local: PathBuf, path: String },
    /// Print the last lines of a file on the SD card, such as a log; with -f, keep printing what's added.
    Tail {
        path: String,
        /// How many lines.
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: usize,
        /// Keep printing what's added until Ctrl-C.
        #[arg(short, long)]
        follow: bool,
    },
    /// Let agents use FOLDER on the SD card from now on without asking (reading; --write for writing and
    /// deleting too). Only works typed in a terminal. Without FOLDER, list what's allowed.
    Allow {
        folder: Option<String>,
        #[arg(long)]
        write: bool,
        /// Stop allowing FOLDER instead.
        #[arg(long)]
        remove: bool,
    },
    /// Delete a file or directory on the SD card (never in Atmosphère's or hekate's folders).
    Rm {
        path: String,
        /// Delete a directory and everything in it.
        #[arg(short, long)]
        recursive: bool,
    },
    /// Manage the sysmodule: status, install <nsp>, start, stop, handover (start it and end the NRO), enable/disable (start at boot).
    Sysmodule {
        #[command(subcommand)]
        action: SysmoduleCmd,
    },
    /// End the NRO daemon (back to hbmenu), as if + were pressed.
    Exit,
    /// Debug: read the IRAM page at OFFSET (hex ok) from the sysmodule and hexdump its start; with
    /// --compare FILE, also count how many bytes match FILE at the same offset from 0x10000 (the payload).
    #[command(hide = true)]
    IramPeek {
        offset: String,
        #[arg(long)]
        compare: Option<PathBuf>,
    },
    /// Press buttons on a virtual controller: BUTTONS like "a", "home" or "zl+zr" (pressed together).
    /// "unlock" presses B three times, which dismisses the lock screen (B, unlike A, does nothing on
    /// the home menu if the console was already unlocked).
    Press {
        buttons: String,
        /// How many times to press them.
        #[arg(short = 'n', long, default_value_t = 1)]
        times: u8,
        /// How long each press is held, in milliseconds.
        #[arg(long, default_value_t = 100)]
        hold_ms: u32,
    },
    /// Play timed input on one virtual controller: STEPS like "r:300 r+a:150 r:6000 -:500", each the
    /// buttons held together ("-" for none) and for how many milliseconds; 20 s at most in all.
    Input { steps: String },
    /// Record controller input as a macro (play it on the Switch while this records), replay one, or
    /// list and delete them. Saved in ~/.config/kickstand/macros/.
    Macro {
        #[command(subcommand)]
        action: MacroCmd,
    },
    /// Type TEXT on a virtual USB keyboard (into the software keyboard, for one).
    Type {
        text: String,
        /// Press Enter afterwards.
        #[arg(long)]
        enter: bool,
    },
    /// Find files and folders under DIR whose name contains NAME (ignoring case), searched on the console.
    Find {
        dir: String,
        #[arg(default_value = "")]
        name: String,
        /// Most results.
        #[arg(long, default_value_t = 200)]
        max: u32,
    },
    /// Print the lines of a file, or of every file under a folder, containing TEXT, searched on the console.
    Grep {
        text: String,
        path: String,
        /// Ignore ASCII case.
        #[arg(short, long)]
        ignore_case: bool,
        /// Most matching lines.
        #[arg(long, default_value_t = 100)]
        max: u32,
    },
    /// The running application, if any, and whether it's hbloader (hbmenu or an NRO).
    App,
    /// Where the console is: uptime, unlock presses after a reboot, keep-awake, application, applet.
    Status,
    /// Quit the running application as from the home menu (HOME, Close software), so it can save first; with
    /// --applet, the Album (with HOME).
    #[command(alias = "close-app")]
    Quit {
        #[arg(long)]
        applet: bool,
    },
    /// Terminate the running application at once (it can't save; the home menu shows an error).
    ForceQuit,
    /// Installed titles, most recently used first (the home menu's order).
    Apps {
        #[arg(long, default_value_t = proto::HOME_MENU_TILES as u16)]
        max: u16,
    },
    /// Start an installed title from the home menu: TITLE is its id or part of its name.
    Launch {
        title: String,
        /// Which user to pick if it asks (0 = the first).
        #[arg(long, default_value_t = 0)]
        user: usize,
    },
    /// Open hbmenu in applet mode (the home menu's Album button), or with --full, with full memory by
    /// starting hbloader in place of a title (the saved host title, or the first on the home menu's row that works;
    /// closes any running title first).
    Hbmenu {
        #[arg(long)]
        full: bool,
    },
    /// Run an NRO from hbmenu with full memory (or --applet for small tools; refused for games) and stream
    /// its stdout until it exits. NRO is a file on this PC, or sdmc:/path for one on the SD card.
    Run {
        nro: String,
        #[arg(long)]
        applet: bool,
        /// Where a local NRO goes on the SD card (default /switch/kickstand/<file name>).
        #[arg(long)]
        dest: Option<String>,
        /// The NRO's ELF, to symbolize `<module>+0x...` backtrace addresses in its output.
        #[arg(long)]
        elf: Option<PathBuf>,
        /// The module name its backtraces use (default: the ELF's file stem).
        #[arg(long)]
        module: Option<String>,
        /// The NRO's arguments, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Hot-reload an NRO into hbmenu's NetLoader without a full Home Menu round-trip, and stream its stdout.
    Reload {
        nro: String,
        #[arg(long)]
        applet: bool,
        /// Where a local NRO goes on the SD card (default /switch/kickstand/<file name>).
        #[arg(long)]
        dest: Option<String>,
        /// Prefer USB transfer when a cable or SD mount is present (default true; pass --no-usb for Wi-Fi).
        #[arg(long)]
        no_usb: bool,
        /// Graceful in-app exit button sequence (e.g. "plus") to try before falling back to hbmenu re-entry.
        #[arg(long)]
        exit_buttons: Option<String>,
        /// Saved macro name to replay automatically after reload completes.
        #[arg(long)]
        replay_macro: Option<String>,
        /// The NRO's ELF, to symbolize `<module>+0x...` backtrace addresses in its output.
        #[arg(long)]
        elf: Option<PathBuf>,
        /// The module name its backtraces use (default: the ELF's file stem).
        #[arg(long)]
        module: Option<String>,
        /// The NRO's arguments, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Sample the running application's call stacks (50-100 Hz), generate a `.folded` stack file and `.svg`
    /// flamegraph, and print a hotspot report.
    Profile {
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
        #[arg(long, default_value_t = 60)]
        hz: u32,
        #[arg(long)]
        thread_id: Option<String>,
        #[arg(long)]
        elf: Option<PathBuf>,
        #[arg(long)]
        module: Option<String>,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long)]
        svg: Option<PathBuf>,
    },
    /// Capture and analyze console audio (`replay`, `record`), or inspect/set system audio volume (`status`, `control`).
    Audio {
        #[command(subcommand)]
        action: AudioCmd,
    },
    /// Symbolize `<module>+0x<offset>` addresses in TEXT (or stdin) against ELF.
    Symbolize {
        elf: PathBuf,
        text: Option<String>,
        #[arg(long)]
        module: Option<String>,
    },
    /// List the newest crash reports and fatal records.
    Crashes {
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    /// Show a crash report (summarized) or fatal record; NAME as `crashes` lists it.
    Crash {
        name: String,
        /// The crashed module's ELF, to symbolize its addresses.
        #[arg(long)]
        elf: Option<PathBuf>,
        #[arg(long)]
        module: Option<String>,
        /// Also save Atmosphère's screenshot of the crash here.
        #[arg(long)]
        screenshot: Option<PathBuf>,
    },
    /// Wait until the screen changes ("change") or stops changing ("still"), then save it with --out.
    Wait {
        #[arg(default_value = "change")]
        until: String,
        #[arg(long, default_value_t = 10.0)]
        timeout: f64,
        /// Only watch "x,y,w,h" of the screen.
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Every thread's backtrace in the running homebrew (paused for a moment): where each thread is, named
    /// from --elf with source lines when it has DWARF. For a hang: which thread waits on what.
    Threads {
        #[arg(long)]
        elf: Option<PathBuf>,
        #[arg(long)]
        module: Option<String>,
    },
    /// Build, deploy, run and watch a project from ~/.config/kickstand/projects.json, e.g. `{"mygame":
    /// {"dir": "...", "build": "cmake --build build-switch --target mygame_nro", "nro":
    /// "build-switch/mygame.nro", "elf": "build-switch/mygame.elf", "dest":
    /// "/switch/mygame/mygame.nro"}}` (nro and elf relative to dir; optional "args", "module").
    Test {
        project: String,
        #[arg(long)]
        no_build: bool,
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        #[arg(long, default_value_t = 20)]
        quiet: u64,
        /// Save the screenshot (crash or hang) here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Arguments for the NRO, after `--` (default: the project's).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// List the projects in ~/.config/kickstand/projects.json.
    Projects,
    /// Watch the running homebrew until it exits, crashes, takes the console down (fatal: reboots into hekate
    /// entry 1 unless --no-reboot) or hangs (an unchanged screen for --quiet s + 5 s: thread backtraces), or
    /// --seconds pass.
    Supervise {
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        #[arg(long, default_value_t = 20)]
        quiet: u64,
        #[arg(long)]
        elf: Option<PathBuf>,
        #[arg(long)]
        no_reboot: bool,
        /// Save the screenshot (crash or hang) here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// The running application's memory map, and where the homebrew's NRO is in it.
    Memory {
        /// The NRO's ELF, to name what regions start with.
        #[arg(long)]
        elf: Option<PathBuf>,
    },
    /// Read the running application's memory. ADDRESS is 0x…, nro+0x…, heap+0x…, a symbol (with --elf),
    /// any of those plus or minus offsets, or [ADDRESS] for the pointer stored there.
    Peek {
        address: String,
        /// Bytes to read (default: one value, 64 for hex, 256 for str).
        #[arg(long, short = 'n', default_value_t = 0)]
        len: usize,
        /// hex, u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, ptr, str.
        #[arg(long = "as", default_value = "hex")]
        format: String,
        #[arg(long)]
        elf: Option<PathBuf>,
        #[arg(long)]
        module: Option<String>,
    },
    /// Write to the running homebrew's memory (refused for games): VALUE as --as says (hex: bytes).
    Poke {
        address: String,
        value: String,
        #[arg(long = "as")]
        format: String,
        #[arg(long)]
        elf: Option<PathBuf>,
        #[arg(long)]
        module: Option<String>,
    },
    /// Serve a page showing the console's screen live, for a person to follow along.
    Watch {
        #[arg(long, default_value_t = 8790)]
        http_port: u16,
        /// 0.0.0.0 to watch from another device on the network.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 4)]
        fps: u32,
    },
    /// Keep the screen on for MINUTES (max 480) with no requests coming in; 0 ends it now, no argument
    /// reports what's left. Every request already keeps it on for 30 minutes.
    KeepAwake { minutes: Option<u32> },
    /// Save a screenshot of the console (JPEG).
    Screenshot { local: PathBuf },
    /// Record the screen as a looping GIF (and an .mp4 of the console's own video when the running game
    /// records gameplay and ffmpeg is installed; otherwise from repeated screenshots).
    Gif {
        local: PathBuf,
        /// Use screenshots even when the console's video stream would work.
        #[arg(long)]
        screenshots: bool,
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
        /// Target frame rate; capturing is slower than this at high rates.
        #[arg(long, default_value_t = 5)]
        fps: u32,
        /// Frame width in pixels (the console's is 1280).
        #[arg(long, default_value_t = 640)]
        width: u32,
    },
    /// Debug: move the "agent working" indicator to X, Y (1920x1080 layer space), until the next boot.
    #[command(hide = true)]
    IndicatorAt { x: u16, y: u16 },
    /// Resource limit usage of the daemon's group (for the sysmodule: all system processes)
    Resources,
    /// Warm-reboot into hekate: TARGET is "menu", "ums", or a hekate entry name or number. Without
    /// --yes, only lists the entries and says what would happen.
    Reboot {
        target: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Serve a local directory as a fake SD card, for testing without the console.
    #[command(hide = true)]
    Mock {
        root: PathBuf,
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Act as a console showing this pairing code.
        #[arg(long)]
        pair_code: Option<String>,
        /// The console's name, as discovery reports it.
        #[arg(long, default_value = "Mock Switch")]
        name: String,
    },
}

#[derive(Subcommand)]
enum AudioCmd {
    /// What the console's background audio capture is doing, and why it hears nothing if it doesn't.
    Status,
    /// Save the last N seconds (up to 5.0 s) the console has been recording in the background, and analyse them.
    Replay {
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        reference: Option<PathBuf>,
    },
    /// Record the next N seconds (up to 30.0 s) of console audio as it plays, and analyse them.
    Record {
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        reference: Option<PathBuf>,
    },
    /// Query or modify system audio output volume (0-100) and mute state.
    Control {
        #[arg(long)]
        volume: Option<u8>,
        #[arg(long)]
        muted: Option<bool>,
    },
}

#[derive(Subcommand)]
enum MacroCmd {
    /// Record until Enter is pressed here (or --seconds pass), then save it as NAME.
    Record {
        name: String,
        /// What it does and where it starts ("title screen to Rogueport, file 1").
        #[arg(long, default_value = "")]
        note: String,
        /// Stop by itself after this many seconds.
        #[arg(long, default_value_t = 300)]
        seconds: u32,
    },
    /// Save STEPS (in `input`'s syntax, e.g. "a:100 -:2500 l=0,1:1800") as macro NAME.
    Save {
        name: String,
        steps: String,
        /// What it does and where it starts.
        #[arg(long, default_value = "")]
        note: String,
        /// Replace a macro already called NAME.
        #[arg(long)]
        replace: bool,
    },
    /// Replay macro NAME (Ctrl-C stops it and lets go of everything).
    Play { name: String },
    /// Pause the macro playing now, whoever plays it: everything is let go until `resume` (or `stop`).
    Pause,
    /// Carry on with a paused macro from where it was.
    Resume,
    /// Stop the macro playing now, whoever plays it, and let go of everything.
    Stop,
    /// List the saved macros.
    List,
    /// Delete macro NAME.
    Delete { name: String },
}

#[derive(Subcommand)]
enum SysmoduleCmd {
    Status,
    /// Upload sys-kickstand.nsp and the token, then restart the sysmodule onto it without a reboot (through
    /// the Kickstand NRO in applet mode; --nro sends a newer one first). Doesn't enable start-at-boot.
    Install {
        nsp: PathBuf,
        #[arg(long)]
        nro: Option<PathBuf>,
        /// Only upload; the new sysmodule runs from the next start or reboot.
        #[arg(long)]
        no_restart: bool,
    },
    Start,
    Stop,
    Handover,
    /// Start it at every boot (creates boot2.flag).
    Enable,
    /// Stop starting it at boot (deletes boot2.flag).
    Disable,
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    let command = cli.command.take().unwrap_or(if stdin_is_pipe() { Command::Serve { app: None } } else { Command::Desktop });
    update::clean_up();
    if let Command::Desktop = command {
        hide_own_console();
        return desktop::run();
    }
    let json = cli.json;
    match run(cli, command) {
        Err(e) if json => {
            out::Out::error(&e);
            std::process::exit(1);
        }
        other => other,
    }
}

/// Windows: opened from Explorer, the program gets a console window of its own, empty behind the desktop
/// app's. It goes when this is the only process using it (a terminal's console is left alone).
fn hide_own_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
        let mut processes = [0u32; 2];
        // SAFETY: the buffer holds the 2 ids it's told about.
        if unsafe { GetConsoleProcessList(processes.as_mut_ptr(), 2) } == 1 {
            // SAFETY: nothing else in this process uses the console (the desktop app doesn't print).
            unsafe { FreeConsole() };
        }
    }
}

/// Whether an MCP client started this, talking over stdin (configs from before `serve` was spelled out).
fn stdin_is_pipe() -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        use std::os::unix::fs::FileTypeExt;
        let stdin = std::io::stdin().as_fd().try_clone_to_owned().map(std::fs::File::from);
        stdin.and_then(|f| f.metadata()).is_ok_and(|m| m.file_type().is_fifo() || m.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        use std::io::IsTerminal;
        !std::io::stdin().is_terminal()
    }
}

#[tokio::main]
async fn run(cli: Cli, command: Command) -> Result<()> {
    let out = out::Out::new(cli.json);
    if out.json() && matches!(command, Command::Serve { .. } | Command::Mock { .. } | Command::Watch { .. }) {
        anyhow::bail!("--json is for commands that finish with a result; serve, mock and watch keep running");
    }
    match command {
        Command::Symbolize { elf, text, module } => {
            let symbols = symbolize::Symbolizer::new(&elf, module.as_deref())?;
            let text = match text {
                Some(t) => t,
                None => std::io::read_to_string(std::io::stdin())?,
            };
            let text = symbols.annotate(&text);
            out.done(json!({ "text": text }), || text.clone());
            return Ok(());
        }
        Command::Token => {
            let (token, path) = config::ensure_token()?;
            out.done(json!({ "token": token, "path": path, "sd_path": proto::path::TOKEN_PATH }), || {
                format!(
                    "token: {token}\nsaved in {}\n`kickstand pair <code>` sends it to a Switch; by hand, add it as a line of {} on the SD card",
                    path.display(),
                    proto::path::TOKEN_PATH
                )
            });
            return Ok(());
        }
        Command::Mock { root, bind, pair_code, name } => {
            let tokens = config::load_token().ok().into_iter().collect();
            let listener = tokio::net::TcpListener::bind((bind.as_str(), cli.port)).await?;
            let probes = tokio::net::UdpSocket::bind((bind.as_str(), proto::discovery::PORT)).await?;
            eprintln!("mock daemon serving {} on {} (discovery on UDP {})", root.display(), listener.local_addr()?, probes.local_addr()?);
            tokio::spawn(mock::answer_probes(probes, cli.port, pair_code.is_some(), name));
            let console = mock::Console::new(root, tokens, pair_code);
            return mock::serve_console(listener, Arc::new(console)).await;
        }
        Command::Link { app, remove } => {
            let Some(app) = app else {
                let apps: Vec<(link::App, link::State)> = link::App::ALL.into_iter().map(|app| (app, link::state(app))).collect();
                let value = apps
                    .iter()
                    .map(|(app, state)| {
                        let (state, command) = match state {
                            link::State::Missing => ("not_installed", None),
                            link::State::NotLinked => ("not_linked", None),
                            link::State::Linked { command } => ("linked", Some(command)),
                        };
                        json!({ "slug": app.slug(), "name": app.name(), "state": state, "command": command })
                    })
                    .collect::<Vec<_>>();
                out.done(json!({ "apps": value }), || {
                    let lines: Vec<String> = apps
                        .iter()
                        .map(|(app, state)| {
                            let state = match state {
                                link::State::Missing => "not installed".to_string(),
                                link::State::NotLinked => "not linked".to_string(),
                                link::State::Linked { command } => format!("linked ({command})"),
                            };
                            format!("{} ({}): {state}", app.name(), app.slug())
                        })
                        .collect();
                    lines.join("\n")
                });
                return Ok(());
            };
            let Some(app) = link::App::from_slug(&app) else {
                let names = link::App::ALL.map(|a| format!("{} ({})", a.slug(), a.name())).join(", ");
                anyhow::bail!("{app:?} isn't one Kickstand knows: {names}. Any other MCP app can run `kickstand serve` itself");
            };
            let message = if remove { link::unlink(app)? } else { link::link(app)? };
            out.done(json!({ "app": app.slug(), "linked": !remove, "message": message }), || message.clone());
            return Ok(());
        }
        Command::Install { remove } => {
            let message = install::run(remove)?;
            out.done(json!({ "installed": !remove, "message": message }), || message.clone());
            return Ok(());
        }
        Command::Allow { folder, write, remove } => {
            let Some(folder) = folder else {
                let saved = access::saved();
                let read: Vec<&String> = saved.read.iter().filter(|f| !saved.write.contains(f)).collect();
                out.done(json!({ "always": proto::path::OWN_DIRS, "write": saved.write, "read": read }), || {
                    let mut lines = vec![format!(
                        "always: Kickstand's own folders ({}), reading crash reports, and the folders projects.json deploys to",
                        proto::path::OWN_DIRS.join(", ")
                    )];
                    lines.extend(saved.write.iter().map(|f| format!("{f}: read and write")));
                    lines.extend(read.iter().map(|f| format!("{f}: read")));
                    lines.join("\n")
                });
                return Ok(());
            };
            use std::io::IsTerminal;
            anyhow::ensure!(std::io::stdin().is_terminal(), "type `kickstand allow` in a terminal yourself: it doesn't run from an agent or a script");
            let folder = check_path(&folder, write)?;
            if remove {
                let was = access::forget(&folder)?;
                out.done(json!({ "folder": folder, "removed": was }), || {
                    if was { format!("{folder} is no longer allowed") } else { format!("{folder} wasn't allowed") }
                });
            } else {
                let need = if write { access::Need::Write } else { access::Need::Read };
                access::save(&folder, need)?;
                out.done(json!({ "folder": folder, "access": if write { "write" } else { "read" } }), || {
                    format!(
                        "agents may {} {} from now on without asking",
                        if write { "read, write and delete in" } else { "read" },
                        access::describe(&folder, access::is_file_scope(&folder, folder.rsplit('/').next().is_none_or(|name| !name.contains('.'))))
                    )
                });
            }
            return Ok(());
        }
        Command::Discover { to } => {
            let to = match to {
                Some(host) => Some(format!("{host}:{}", proto::discovery::PORT).parse().context("--to takes an IPv4 address")?),
                None => None,
            };
            let found = pairing::discover(pairing::DISCOVER_WAIT, to).await?;
            let kind = |k: proto::DaemonKind| match k {
                proto::DaemonKind::Sysmodule => "sysmodule",
                proto::DaemonKind::Nro => "nro",
            };
            let value: Vec<_> = found
                .iter()
                .map(|f| {
                    let a = &f.announce;
                    json!({
                        "host": f.addr.ip().to_string(), "port": f.addr.port(), "nickname": a.nickname,
                        "version": a.daemon_version, "protocol": a.protocol, "kind": kind(a.kind), "pairing": a.pairing,
                    })
                })
                .collect();
            out.done(json!({ "consoles": value }), || {
                if found.is_empty() {
                    return "no Switch running Kickstand answered (is it on the same network as this PC?)".into();
                }
                let lines: Vec<String> = found
                    .iter()
                    .map(|f| {
                        let a = &f.announce;
                        let what = if a.kind == proto::DaemonKind::Sysmodule { "background service" } else { "on-screen app" };
                        let pairing = if a.pairing { ", showing a pairing code" } else { "" };
                        format!("{}: Kickstand {} ({what}, protocol {}){pairing}", f.describe(), a.daemon_version, a.protocol)
                    })
                    .collect();
                lines.join("\n")
            });
            return Ok(());
        }
        Command::Update { check, switch } => return update(&out, cli.host, cli.port, check, switch).await,
        Command::Pair { code } => {
            let host = pairing::pick(cli.host.as_deref(), cli.port).await?;
            let info = pairing::pair(&host, cli.port, &code).await?;
            out.done(json!({ "host": host, "version": info.version }), || {
                format!("paired with the Switch at {host} (Kickstand {}); this PC uses it from now on", info.version)
            });
            return Ok(());
        }
        _ => {}
    }

    let host = config::host(cli.host)?;
    let client = Client::new(format!("{host}:{}", cli.port), config::load_token()?);
    // A person's command (or a script's), not an agent: the MCP server names itself below.
    let client = Arc::new(if matches!(command, Command::Serve { .. }) { client } else { client.named(client::TERMINAL) });

    match command {
        Command::Desktop => unreachable!("opened before the runtime starts"),
        Command::Allow { .. } => unreachable!("handled before connecting"),
        Command::Serve { app } => {
            if let Some(app) = app {
                client.set_app(&app).await;
            }
            let service = tools::Tools::new(client).serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        Command::Unpair => {
            client.unpair().await?;
            let forgot = config::forget_host(&host)?;
            out.done(json!({ "host": host, "forgot_host": forgot }), || {
                format!("the Switch at {host} has forgotten this PC{}", if forgot { "; this PC no longer uses it" } else { "" })
            });
        }
        Command::Ping => {
            let info = client.hello().await?;
            let (rtt, uptime) = client.ping().await?;
            let kind = if info.kind == proto::DaemonKind::Sysmodule { "sysmodule" } else { "nro" };
            out.done(
                json!({ "host": host, "version": info.version, "kind": kind, "protocol": info.protocol, "rtt_ms": rtt.as_secs_f64() * 1e3, "uptime_s": uptime.as_secs() }),
                || format!("daemon {} ({:?}, protocol {}): rtt {:.1} ms, uptime {}s", info.version, info.kind, info.protocol, rtt.as_secs_f64() * 1e3, uptime.as_secs()),
            );
        }
        Command::Ls { path } => {
            let path = check_path(&path, false)?;
            typed_or_allowed(&path, access::Need::Read, true)?;
            let (entries, truncated) = client.list_dir(&path).await?;
            out.done(json!({ "path": path, "entries": entries, "truncated": truncated }), || {
                let mut lines: Vec<String> = entries.iter().map(|e| if e.is_dir { format!("{}/", e.name) } else { format!("{}\t{}", e.name, e.size) }).collect();
                if truncated {
                    lines.push("... (truncated)".into());
                }
                lines.join("\n")
            });
        }
        Command::Cat { path } => {
            let path = check_path(&path, false)?;
            typed_or_allowed(&path, access::Need::Read, false)?;
            let size = client.stat(&path).await?.size;
            let (data, _) = client.read(&path, 0, size).await?;
            if out.json() {
                use base64::Engine;
                let value = match std::str::from_utf8(&data) {
                    Ok(text) => json!({ "path": path, "size": data.len(), "text": text }),
                    Err(_) => json!({ "path": path, "size": data.len(), "base64": base64::engine::general_purpose::STANDARD.encode(&data) }),
                };
                out.done(value, String::new);
            } else {
                use std::io::Write;
                std::io::stdout().write_all(&data)?;
            }
        }
        Command::Get { path, local } => {
            let path = check_path(&path, false)?;
            typed_or_allowed(&path, access::Need::Read, false)?;
            let size = client.stat(&path).await?.size;
            let start = std::time::Instant::now();
            let (data, _) = client.read(&path, 0, size).await?;
            let secs = start.elapsed().as_secs_f64();
            std::fs::write(&local, &data).with_context(|| format!("writing {}", local.display()))?;
            out.done(json!({ "path": path, "local": local, "bytes": data.len(), "seconds": secs }), || {
                format!("{} bytes in {secs:.1}s ({}) -> {}", data.len(), mb_per_s(data.len() as u64, secs), local.display())
            });
        }
        Command::Put { local, path, usb: false } => {
            let path = check_path(&path, true)?;
            typed_or_allowed(&path, access::Need::Write, false)?;
            let stats = client.upload_file(&path, &local).await?;
            let secs = stats.elapsed.as_secs_f64();
            out.done(json!({ "local": local, "path": path, "bytes": stats.size, "seconds": secs }), || {
                format!("{} bytes in {secs:.1}s ({})", stats.size, mb_per_s(stats.size, secs))
            });
        }
        Command::Put { local, path, usb: true } | Command::UsbPut { local, path } => {
            let path = check_path(&path, true)?;
            typed_or_allowed(&path, access::Need::Write, false)?;
            let stats = usb::upload(&client, &local, &path).await?;
            let secs = stats.elapsed.as_secs_f64();
            out.done(json!({ "local": local, "path": path, "via": stats.via, "bytes": stats.size, "seconds": secs }), || {
                format!("{} bytes via {} in {secs:.1}s ({})", stats.size, stats.via, mb_per_s(stats.size, secs))
            });
        }
        Command::Tail { path, lines, follow } => {
            use std::io::Write;
            let path = check_path(&path, false)?;
            typed_or_allowed(&path, access::Need::Read, false)?;
            let (text, mut next) = tail::last_lines(&client, &path, lines).await?;
            if !follow {
                out.done(json!({ "path": path, "text": text, "next": next }), || text.clone());
                return Ok(());
            }
            if out.json() {
                out.event(json!({ "path": path, "text": text, "next": next }));
            } else {
                print!("{text}");
            }
            loop {
                tokio::time::sleep(tail::POLL).await;
                let (data, after, restarted) = tail::since(&client, &path, next).await?;
                next = after;
                if out.json() {
                    if restarted || !data.is_empty() {
                        out.event(json!({ "text": String::from_utf8_lossy(&data), "next": next, "restarted": restarted }));
                    }
                    continue;
                }
                if restarted {
                    eprintln!("({path} got shorter: reading it again from the start)");
                }
                std::io::stdout().write_all(&data)?;
                std::io::stdout().flush()?;
            }
        }
        Command::Rm { path, recursive } => {
            let path = check_path(&path, true)?;
            typed_or_allowed(&path, access::Need::Write, client.stat(&path).await.is_ok_and(|e| e.is_dir))?;
            let was_dir = client.delete(&path, recursive).await?;
            out.done(json!({ "path": path, "was_dir": was_dir }), || format!("deleted {path}{}", if was_dir { "/" } else { "" }));
        }
        Command::Macro { action } => match action {
            MacroCmd::Record { name, note, seconds } => {
                let name = macros::check_name(&name)?.to_string();
                let seconds = seconds.clamp(1, proto::MAX_RECORD_MS / 1000);
                client.record_start(seconds * 1000).await?;
                eprintln!("recording: play on the Switch now, then press Enter here (stops by itself after {seconds} s)");
                let mut line = String::new();
                let enter = tokio::task::spawn_blocking(move || std::io::stdin().read_line(&mut line));
                let _ = tokio::time::timeout(std::time::Duration::from_secs(u64::from(seconds)), enter).await;
                let (inputs, ms, full) = client.record_stop().await?;
                let data = macros::from_recording(&inputs, ms, note)?;
                let path = macros::save(&name, &data)?;
                let steps = data.steps.split_whitespace().count();
                out.done(json!({ "name": name, "path": path, "ms": data.ms, "steps": steps, "full": full, "macro": data }), || {
                    format!(
                        "saved {name:?}: {}, {steps} steps ({}){}",
                        macros::length(data.ms),
                        path.display(),
                        if full { "; the recording filled up and stopped early" } else { "" }
                    )
                });
            }
            MacroCmd::Save { name, steps, note, replace } => {
                let name = macros::check_name(&name)?.to_string();
                let data = macros::from_steps(&steps, note, macros::player(Some(crate::client::TERMINAL)))?;
                let path = macros::create(&name, &data, replace)?;
                let count = data.steps.split_whitespace().count();
                out.done(json!({ "name": name, "path": path, "ms": data.ms, "steps": count }), || {
                    format!("saved {name:?}: {}, {count} steps ({})", macros::length(data.ms), path.display())
                });
            }
            MacroCmd::Play { name } => {
                let play = macros::play(&client, &name);
                tokio::pin!(play);
                let replayed = tokio::select! {
                    replayed = &mut play => replayed?,
                    _ = tokio::signal::ctrl_c() => {
                        // `play` holds the connection; it returns as soon as the console stops.
                        client.another().playback(proto::PlaybackAction::Stop).await?;
                        play.await?
                    }
                };
                out.done(replayed.json(), || replayed.summary());
            }
            MacroCmd::Pause => control_macro(&out, &client, proto::PlaybackAction::Pause).await?,
            MacroCmd::Resume => control_macro(&out, &client, proto::PlaybackAction::Resume).await?,
            MacroCmd::Stop => control_macro(&out, &client, proto::PlaybackAction::Stop).await?,
            MacroCmd::List => {
                let all = macros::list()?;
                let value: Vec<_> = all.iter().map(|m| json!({ "name": m.name, "recorded": m.data.recorded, "ms": m.data.ms, "note": m.data.note, "steps": m.data.steps, "by": m.data.by })).collect();
                out.done(json!({ "macros": value }), || {
                    all.iter().map(|m| format!("{}\t{}\t{}", m.name, macros::length(m.data.ms), m.data.note)).collect::<Vec<_>>().join("\n")
                });
            }
            MacroCmd::Delete { name } => {
                macros::delete(&name)?;
                out.done(json!({ "name": name }), || format!("deleted {name:?}"));
            }
        },
        Command::Type { text, enter } => {
            let text = if enter { format!("{text}\n") } else { text };
            client.type_text(&text).await?;
            let typed = text.chars().count();
            out.done(json!({ "typed": typed }), || format!("typed {typed} characters"));
        }
        Command::Find { dir, name, max } => {
            let dir = check_path(&dir, false)?;
            typed_or_allowed(&dir, access::Need::Read, true)?;
            let (entries, truncated) = client.find(&dir, &name, max.clamp(1, proto::MAX_FIND)).await?;
            if truncated && !out.json() {
                eprintln!("(stopped early: more than {max}, out of time, or a folder over 1024 entries)");
            }
            out.done(json!({ "dir": dir, "entries": entries, "truncated": truncated }), || {
                entries.iter().map(|e| if e.is_dir { format!("{}/", e.name) } else { format!("{}\t{}", e.name, e.size) }).collect::<Vec<_>>().join("\n")
            });
        }
        Command::Grep { text, path, ignore_case, max } => {
            let path = check_path(&path, false)?;
            let is_dir = client.stat(&path).await?.is_dir;
            typed_or_allowed(&path, access::Need::Read, is_dir)?;
            let (matches, files, truncated) = client.grep(&path, &text, ignore_case, max.clamp(1, proto::MAX_GREP)).await?;
            if !out.json() {
                eprintln!("{} matching lines in {files} file(s){}", matches.len(), if truncated { "; stopped early" } else { "" });
            }
            out.done(json!({ "path": path, "matches": matches, "files": files, "truncated": truncated }), || {
                let lines: Vec<String> =
                    matches.iter().map(|m| if is_dir { format!("{}:{}: {}", m.path, m.line, m.text) } else { format!("{}: {}", m.line, m.text) }).collect();
                lines.join("\n")
            });
        }
        Command::Sysmodule { action } => {
            let status = match action {
                SysmoduleCmd::Status => client.sysmodule(proto::Request::SysmoduleStatus).await?,
                SysmoduleCmd::Start => client.sysmodule(proto::Request::SysmoduleStart).await?,
                SysmoduleCmd::Stop => client.sysmodule(proto::Request::SysmoduleStop).await?,
                SysmoduleCmd::Handover => client.handover().await?,
                SysmoduleCmd::Enable => client.set_boot2(true).await?,
                SysmoduleCmd::Disable => client.set_boot2(false).await?,
                SysmoduleCmd::Install { nsp, nro, no_restart } => {
                    let data = std::fs::read(&nsp).with_context(|| format!("reading {}", nsp.display()))?;
                    let nro = nro.map(|p| std::fs::read(&p).with_context(|| format!("reading {}", p.display()))).transpose()?;
                    client.install_sysmodule(&data).await?;
                    if !no_restart {
                        for note in launch::restart_sysmodule(&client, nro).await? {
                            out.note(note);
                        }
                    }
                    client.sysmodule(proto::Request::SysmoduleStatus).await?
                }
            };
            out.done(json!({ "installed": status.installed, "boot2": status.boot2, "pid": status.pid }), || format!("{status:?}"));
        }
        Command::Reboot { target, yes } => {
            let entries = client.hekate_entries().await?;
            let listing = || {
                let mut lines = vec!["hekate entries:".to_string()];
                lines.extend(entries.iter().enumerate().map(|(i, e)| format!("  {}. {e}", i + 1)));
                lines.join("\n")
            };
            let Some(target) = target else {
                out.done(json!({ "entries": entries }), listing);
                return Ok(());
            };
            let target = client::parse_hekate_target(&target, &entries)?;
            let what = tools::describe_target(target, &entries);
            if !yes {
                out.done(json!({ "entries": entries, "target": what, "rebooting": false }), || {
                    format!("{}\nwould reboot into hekate: {what} (add --yes to do it)", listing())
                });
                return Ok(());
            }
            client.reboot_to_hekate(target).await?;
            out.done(json!({ "entries": entries, "target": what, "rebooting": true }), || format!("{}\nrebooting into hekate: {what}", listing()));
        }
        Command::IramPeek { offset, compare } => {
            let offset = match offset.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16)?,
                None => offset.parse()?,
            };
            let page = client.iram_peek(offset).await?;
            let mut lines = Vec::new();
            for (i, row) in page.chunks(16).take(8).enumerate() {
                let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
                lines.push(format!("{:#07x}: {}", 0x4000_0000u32 + offset + i as u32 * 16, hex.join(" ")));
            }
            if let Some(file) = compare {
                let data = std::fs::read(&file)?;
                let start = (offset as usize).checked_sub(0x10000).context("compare needs an offset of at least 0x10000")?;
                let want = data.get(start..(start + page.len()).min(data.len())).unwrap_or(&[]);
                let same = want.iter().zip(&page).filter(|(a, b)| a == b).count();
                lines.push(format!("{same} of {} bytes match {} at {start:#x}", want.len(), file.display()));
            }
            let text = lines.join("\n");
            out.done(json!({ "text": text }), || text.clone());
        }
        Command::Press { buttons, times, hold_ms } => {
            let (parsed, times) = match buttons.as_str() {
                "unlock" => (vec![proto::Button::B], 3),
                spec => (client::parse_buttons(spec)?, times),
            };
            client.press(parsed, times, hold_ms).await?;
            out.done(json!({ "buttons": buttons, "times": times }), || "pressed".into());
        }
        Command::Input { steps } => {
            let parsed = client::parse_steps(&steps)?;
            let count = parsed.len();
            client.input(parsed).await?;
            out.done(json!({ "steps": count }), || "played".into());
        }
        Command::Status => {
            let fatal = client.fatal().await?;
            let status = client.status().await?;
            let mut text = client::describe_status(&status);
            let usb_desc = usb::describe();
            if let Some(usb) = &usb_desc {
                text.push('\n');
                text.push_str(usb);
            }
            out.done(
                json!({
                    "fatal": fatal, "uptime_ms": status.uptime_ms, "unlocking_ms": status.unlocking_ms, "keep_awake_s": status.keep_awake_s,
                    "application": status.application.as_ref().map(out::app), "applet": status.applet.as_ref().map(out::app),
                    "usb": usb_desc.is_some(), "text": text,
                }),
                || match &fatal {
                    Some(fatal) => format!("{fatal}\n{text}"),
                    None => text.clone(),
                },
            );
        }
        Command::App => {
            let app = client.app_status().await?;
            let applet = client.applet_status().await?;
            out.done(json!({ "application": app.as_ref().map(out::app), "applet": applet.as_ref().map(out::app) }), || {
                let app = match &app {
                    Some(app) => format!("application: {}", client::describe_app(app)),
                    None => "application: none".into(),
                };
                let applet = match &applet {
                    Some(a) => format!("applet: the Album, pid {}{}", a.pid, if a.hbl == Some(true) { ", hbloader" } else { "" }),
                    None => "applet: the Album isn't open".into(),
                };
                format!("{app}\n{applet}")
            });
        }
        Command::Quit { applet } => {
            let mode = if applet { launch::Mode::Applet } else { launch::Mode::Full };
            closed(&out, launch::close_running(&client, mode, launch::How::Quit).await?, applet);
        }
        Command::ForceQuit => closed(&out, launch::close_running(&client, launch::Mode::Full, launch::How::Force).await?, false),
        Command::Apps { max } => {
            let apps = client.list_apps(max).await?;
            let value: Vec<_> = apps.iter().map(|a| json!({ "id": out::id(a.id), "name": a.name })).collect();
            out.done(json!({ "apps": value }), || {
                apps.iter().enumerate().map(|(i, app)| format!("{i:>3}  {:016x}  {}", app.id, app.name)).collect::<Vec<_>>().join("\n")
            });
        }
        Command::Watch { http_port, bind, fps } => watch::serve(client.clone(), &bind, http_port, fps).await?,
        Command::Launch { title, user } => {
            let (app, notes) = launch::launch_title(&client, &title, user).await?;
            notes.into_iter().for_each(|n| out.note(n));
            out.done(json!({ "id": out::id(app.id), "name": app.name }), || format!("{} ({:016x}) is running", app.name, app.id));
        }
        Command::Hbmenu { full } => {
            let opened = launch::open_hbmenu(&client, if full { launch::Mode::Full } else { launch::Mode::Applet }).await?;
            opened.notes.iter().for_each(|n| out.note(n.clone()));
            let host_title = opened.host.as_ref().map(|h| json!({ "id": out::id(h.id), "name": h.name }));
            out.done(json!({ "mode": if opened.host.is_some() { "full" } else { "applet" }, "host_title": host_title }), || match &opened.host {
                Some(host) => format!("hbmenu is open with full memory (host title {:016x} {})", host.id, host.name),
                None => "hbmenu is open in applet mode".into(),
            });
        }
        Command::Crashes { limit } => {
            let all = crash::list(&client, limit).await?;
            let value: Vec<_> = all.iter().map(|e| json!({ "path": e.path, "headline": e.headline })).collect();
            out.done(json!({ "reports": value }), || all.iter().map(|e| format!("{}\n    {}", e.path, e.headline)).collect::<Vec<_>>().join("\n"));
        }
        Command::Crash { name, elf, module, screenshot } => {
            let fetched = crash::fetch(&client, &name).await?;
            let symbols = elf.map(|e| symbolize::Symbolizer::new(&e, module.as_deref())).transpose()?;
            let text = crash::describe(&fetched, symbols.as_ref())?;
            let saved = match (screenshot, &fetched.jpg) {
                (Some(path), Some(jpg)) => {
                    std::fs::write(&path, jpg)?;
                    Some(path)
                }
                _ => None,
            };
            out.done(json!({ "name": name, "text": text, "screenshot": saved }), || match &saved {
                Some(path) => format!("{text}screenshot saved to {}", path.display()),
                None => text.clone(),
            });
        }
        Command::Wait { until, timeout, region, out: local } => {
            let until = if until == "still" { capture::Until::Still } else { capture::Until::Change };
            let region = region.as_deref().map(parse_region).transpose()?;
            let waited = capture::wait(&client, until, region, std::time::Duration::from_secs_f64(timeout.clamp(0.0, capture::MAX_WAIT_SECONDS))).await?;
            if let Some(local) = &local {
                std::fs::write(local, &waited.jpeg)?;
            }
            out.done(json!({ "met": waited.met, "seconds": waited.elapsed.as_secs_f64(), "captures": waited.captures, "screenshot": local }), || {
                format!("{} after {:.1} s, {} captures", if waited.met { "met" } else { "timed out" }, waited.elapsed.as_secs_f64(), waited.captures)
            });
        }
        Command::Test { project, no_build, seconds, quiet, out: local, args } => {
            let settings = watchdog::Settings {
                max: std::time::Duration::from_secs(seconds),
                quiet: std::time::Duration::from_secs(quiet.max(1)),
                reboot_on_fatal: true,
            };
            let output = Arc::new(std::sync::Mutex::new(launch::OutputLog::default()));
            let args = (!args.is_empty()).then_some(args);
            let run = project::test(&client, &project::get(&project)?, no_build, args, &output, &settings).await?;
            let saved = save_screenshot(local, run.screenshot)?;
            out.done(json!({ "project": project, "outcome": out::outcome(&run.text), "text": run.text, "screenshot": saved }), || with_screenshot(&run.text, &saved));
        }
        Command::Projects => {
            let all = project::load_all()?;
            let value: serde_json::Map<String, serde_json::Value> = all
                .iter()
                .map(|(name, p)| (name.clone(), json!({ "dir": p.dir, "build": p.build, "nro": p.nro, "elf": p.elf, "dest": p.dest, "args": p.args, "module": p.module })))
                .collect();
            out.done(json!({ "projects": value }), || {
                let lines: Vec<String> = all
                    .iter()
                    .map(|(name, p)| format!("{name}: {} (build: {}; {} -> {})", p.dir.display(), p.build.as_deref().unwrap_or("none"), p.nro.display(), p.dest))
                    .collect();
                lines.join("\n")
            });
        }
        Command::Supervise { seconds, quiet, elf, no_reboot, out: local } => {
            let settings = watchdog::Settings {
                max: std::time::Duration::from_secs(seconds),
                quiet: std::time::Duration::from_secs(quiet.max(1)),
                reboot_on_fatal: !no_reboot,
            };
            let report = watchdog::supervise(&client, None, &memory::Elf::load(elf.as_deref(), None)?, &settings).await?;
            let saved = save_screenshot(local, report.screenshot)?;
            out.done(json!({ "outcome": out::outcome(&report.text), "text": report.text, "screenshot": saved }), || with_screenshot(&report.text, &saved));
        }
        Command::Threads { elf, module } => {
            let text = memory::threads(&client, &memory::Elf::load(elf.as_deref(), module.as_deref())?).await?;
            out.done(json!({ "text": text }), || text.clone());
        }
        Command::Memory { elf } => {
            let text = memory::map(&client, &memory::Elf::load(elf.as_deref(), None)?).await?;
            out.done(json!({ "text": text }), || text.clone());
        }
        Command::Peek { address, len, format, elf, module } => {
            let elf = memory::Elf::load(elf.as_deref(), module.as_deref())?;
            let text = memory::peek(&client, &elf, &address, len, memory::Format::parse(&format)?).await?;
            out.done(json!({ "address": address, "text": text }), || text.clone());
        }
        Command::Poke { address, value, format, elf, module } => {
            let elf = memory::Elf::load(elf.as_deref(), module.as_deref())?;
            let text = memory::poke(&client, &elf, &address, &value, memory::Format::parse(&format)?).await?;
            out.done(json!({ "address": address, "text": text }), || text.clone());
        }
        Command::Run { nro, applet, dest, elf, module, args } => {
            let program = match nro.strip_prefix("sdmc:") {
                Some(path) => {
                    let path = check_path(path, false)?;
                    typed_or_allowed(&path, access::Need::Read, false)?;
                    launch::Program::Sd(path)
                }
                None => {
                    let name = std::path::Path::new(&nro).file_name().context("NRO has no file name")?.to_string_lossy();
                    let dest = check_path(&dest.unwrap_or_else(|| format!("/switch/kickstand/{name}")), true)?;
                    typed_or_allowed(&dest, access::Need::Write, false)?;
                    launch::Program::Local { nro: std::fs::read(&nro).with_context(|| format!("reading {nro}"))?, dest }
                }
            };
            let symbols = elf.map(|e| symbolize::Symbolizer::new(&e, module.as_deref()).map(symbolize::Symbolizer::shared)).transpose()?;
            let mode = if applet { launch::Mode::Applet } else { launch::Mode::Full };
            let started = launch::run_nro(&client, program, args, mode).await?;
            if out.json() {
                started.notes.iter().for_each(|n| out.note(n.clone()));
                out.event(json!({ "started": true, "pid": started.pid, "stdout": started.output.is_some() }));
            } else {
                for note in &started.notes {
                    eprintln!("{note}");
                }
            }
            if let Some(output) = started.output {
                use std::io::Write as _;
                let begun = std::time::Instant::now();
                let mut lines = launch::Lines::new(output, symbols);
                while let Some(line) = lines.next().await {
                    if out.json() {
                        out.event(json!({ "at": begun.elapsed().as_secs_f64(), "line": line.trim_end_matches('\n') }));
                    } else {
                        print!("{line}");
                        let _ = std::io::stdout().flush();
                    }
                }
                let end = lines.end().cloned().unwrap_or(launch::End::Closed);
                if out.json() {
                    let dropped = match &end {
                        launch::End::Dropped(e) => Some(e.clone()),
                        launch::End::Closed => None,
                    };
                    out.event(json!({ "closed": true, "dropped": dropped, "at": begun.elapsed().as_secs_f64() }));
                } else {
                    eprintln!("({})", end.describe());
                }
            }
        }
        Command::Reload { nro, applet, dest, no_usb, exit_buttons, replay_macro, elf, module, args } => {
            let program = match nro.strip_prefix("sdmc:") {
                Some(path) => {
                    let path = check_path(path, false)?;
                    typed_or_allowed(&path, access::Need::Read, false)?;
                    launch::Program::Sd(path)
                }
                None => {
                    let local_path = PathBuf::from(&nro);
                    let name = local_path.file_name().context("NRO has no file name")?.to_string_lossy();
                    let dest = check_path(&dest.unwrap_or_else(|| format!("/switch/kickstand/{name}")), true)?;
                    typed_or_allowed(&dest, access::Need::Write, false)?;
                    let bytes = std::fs::read(&local_path).with_context(|| format!("reading {nro}"))?;
                    launch::Program::Local { nro: bytes, dest }
                }
            };
            let symbols = elf.map(|e| symbolize::Symbolizer::new(&e, module.as_deref()).map(symbolize::Symbolizer::shared)).transpose()?;
            let mode = if applet { launch::Mode::Applet } else { launch::Mode::Full };
            let exit_buttons = exit_buttons.as_deref().map(client::parse_buttons).transpose()?;
            let reloaded = reload::hot_reload(
                &client,
                reload::ReloadOptions {
                    program,
                    args,
                    mode,
                    use_usb: !no_usb,
                    exit_buttons,
                    replay_macro,
                },
            )
            .await?;
            let summary = format!(
                "reloaded ({}) in {:.1}s",
                if reloaded.hot { "hot in-process" } else { "full relaunch" },
                reloaded.elapsed.as_secs_f64()
            );
            if out.json() {
                out.note(summary);
                reloaded.started.notes.iter().for_each(|n| out.note(n.clone()));
                out.event(json!({ "reloaded": true, "hot": reloaded.hot, "pid": reloaded.started.pid, "stdout": reloaded.started.output.is_some() }));
            } else {
                eprintln!("{summary}");
                for note in &reloaded.started.notes {
                    eprintln!("{note}");
                }
            }
            if let Some(output) = reloaded.started.output {
                use std::io::Write as _;
                let begun = std::time::Instant::now();
                let mut lines = launch::Lines::new(output, symbols);
                while let Some(line) = lines.next().await {
                    if out.json() {
                        out.event(json!({ "at": begun.elapsed().as_secs_f64(), "line": line.trim_end_matches('\n') }));
                    } else {
                        print!("{line}");
                        let _ = std::io::stdout().flush();
                    }
                }
                let end = lines.end().cloned().unwrap_or(launch::End::Closed);
                if out.json() {
                    let dropped = match &end {
                        launch::End::Dropped(e) => Some(e.clone()),
                        launch::End::Closed => None,
                    };
                    out.event(json!({ "closed": true, "dropped": dropped, "at": begun.elapsed().as_secs_f64() }));
                } else {
                    eprintln!("({})", end.describe());
                }
            }
        }
        Command::Profile { seconds, hz, thread_id, elf, module, focus: _, svg } => {
            let elf = memory::Elf::load(elf.as_deref(), module.as_deref())?;
            let thread_id = match thread_id.as_deref() {
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
            let duration_ms = (seconds.clamp(0.1, 15.0) * 1000.0).round() as u32;
            let interval_ms = (1000 / hz.clamp(5, 100)).clamp(10, 200);
            let res = profile::run(
                &client,
                &elf,
                &profile::ProfileOptions {
                    duration_ms,
                    interval_ms,
                    thread_id,
                    out: svg,
                    ..Default::default()
                },
            )
            .await?;
            out.done(json!({ "report": res.text, "svg": res.svg_path }), || res.text.clone());
        }
        Command::Audio { action } => match action {
            AudioCmd::Status => {
                let c = client.audio_capture().await?;
                let text = match audio::why_silent(&c) {
                    Some(why) => format!("not capturing: {why}"),
                    None => audio::capture_line(&c),
                };
                out.done(
                    json!({
                        "state": format!("{:?}", c.state),
                        "program_id": c.program_id.map(|id| format!("{id:016x}")),
                        "hbl": c.hbl,
                        "video_capture": c.video_capture,
                        "buffered_ms": c.buffered_ms,
                        "newest_age_ms": c.newest_age_ms,
                        "summary": text,
                    }),
                    || text.clone(),
                );
            }
            AudioCmd::Replay { seconds, out: wav_out, reference } => {
                let (samples, capture) = client.audio_replay(seconds).await?;
                let note = format!(
                    "{} (asked for {seconds:.1} s, got {:.2} s)",
                    audio::capture_line(&capture),
                    samples.len() as f64 / f64::from(audio::DEFAULT_SAMPLE_RATE * u32::from(audio::DEFAULT_CHANNELS))
                );
                save_audio(&out, &samples, wav_out, reference, note)?;
            }
            AudioCmd::Record { seconds, out: wav_out, reference } => {
                let rec = client.record_audio(seconds).await?;
                let mut note = String::new();
                if rec.gaps_ms > 0 {
                    note.push_str(&format!("{} ms of gaps filled with silence", rec.gaps_ms));
                }
                if let Some(why) = &rec.stopped {
                    note.push_str(&format!("{}stopped early: {why}", if note.is_empty() { "" } else { "; " }));
                }
                save_audio(&out, &rec.samples, wav_out, reference, note)?;
            }
            AudioCmd::Control { volume, muted } => {
                let status = client.audio_control(volume, muted).await?;
                out.done(
                    json!({
                        "target": status.target,
                        "sample_rate": status.sample_rate,
                        "channels": status.channels,
                        "volume_pct": status.volume_pct,
                        "muted": status.muted,
                        "output_active": status.output_active,
                    }),
                    || {
                        format!(
                            "audio output: target={}, {} Hz {}ch, volume={}%{}, active={}",
                            status.target,
                            status.sample_rate,
                            status.channels,
                            status.volume_pct,
                            if status.muted { " (muted)" } else { "" },
                            status.output_active
                        )
                    },
                );
            }
        },
        Command::Screenshot { local } => {
            let jpeg = client.screenshot().await?;
            std::fs::write(&local, &jpeg)?;
            out.done(json!({ "path": local, "bytes": jpeg.len() }), || format!("saved {} ({} bytes)", local.display(), jpeg.len()));
        }
        Command::Gif { local, seconds, fps, width, screenshots } => {
            // The console's own video first (also saved as an .mp4), screenshots when it can't.
            let (shots, source) = match if screenshots { Err(anyhow::anyhow!("--screenshots")) } else { video::record_to_file(&client, seconds, &local).await } {
                Ok((mp4, clip_fps, frames, kb)) => {
                    let decode = mp4.clone();
                    let shots = tokio::task::spawn_blocking(move || video::decode_shots(&decode, clip_fps, fps.clamp(1, capture::MAX_FPS), width)).await??;
                    (shots, format!("the console's gameplay video ({frames} frames at {clip_fps:.1} fps, {kb} KB): {}", mp4.display()))
                }
                Err(why) => (capture::record(&client, seconds, fps).await?, format!("screenshots ({why:#})")),
            };
            out.note(format!("from {source}"));
            let sheet_path = local.with_extension("contact_sheet.jpg");
            let sheet_path_clone = sheet_path.clone();
            let (gif, frames, sheet_res) = tokio::task::spawn_blocking(move || -> Result<_> {
                let g = capture::gif(&shots, width)?;
                let s = capture::export_clip(&shots, width, modalities::VisualFormat::ContactSheet, Some(sheet_path_clone))?;
                Ok((g, shots.len(), s))
            }).await??;
            std::fs::write(&local, &gif)?;
            out.done(
                json!({ "path": local, "contact_sheet": sheet_path, "frames": frames, "bytes": gif.len() }),
                || format!("saved {} ({frames} frames, {} bytes)\n{}", local.display(), gif.len(), sheet_res.summary),
            );
        }
        Command::IndicatorAt { x, y } => {
            client.indicator_position(x, y).await?;
            out.done(json!({ "x": x, "y": y }), String::new);
        }
        Command::KeepAwake { minutes } => {
            let left = client.keep_awake(minutes).await?;
            out.done(json!({ "remaining_s": left }), || describe_keep_awake(left));
        }
        Command::Resources => {
            let resources = client.resources().await?;
            out.done(serde_json::to_value(&resources)?, || client::describe_resources(&resources));
        }
        Command::Exit => {
            client.exit().await?;
            out.done(json!({}), || "the NRO daemon is exiting".into());
        }
        Command::Token
        | Command::Mock { .. }
        | Command::Symbolize { .. }
        | Command::Discover { .. }
        | Command::Pair { .. }
        | Command::Update { .. }
        | Command::Link { .. }
        | Command::Install { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

/// What `quit` and `force-quit` closed.
fn closed(out: &out::Out, closed: Option<(proto::App, launch::Closed)>, applet: bool) {
    let value = match &closed {
        Some((app, how)) => json!({ "closed": out::app(app), "how": launch::closed_word(*how) }),
        None => json!({ "closed": null }),
    };
    out.done(value, || match &closed {
        Some((app, how)) => format!("closed {} ({})", client::describe_app(app), launch::describe_closed(*how)),
        None if applet => "the Album wasn't open".into(),
        None => "no application was running".into(),
    });
}

/// `kickstand update`: the PC first; a new copy then updates the Switch itself, with its own version.
async fn update(out: &out::Out, host: Option<String>, port: u16, check: bool, switch_only: bool) -> Result<()> {
    let mut pc_latest = None;
    if !switch_only {
        let newer = tokio::task::spawn_blocking(update::newer).await??;
        match newer {
            None => out.note(format!("Kickstand {} on this PC is the latest", update::VERSION)),
            Some(release) if check => {
                pc_latest = Some(release.version.to_string());
                out.note(format!("Kickstand {} is out (this PC has {}): run `kickstand update`. What's new: {}", release.version, update::VERSION, release.page))
            }
            Some(release) => {
                // Said straight away: the new copy finishes the job and prints its own result.
                let say = |line: String| if out.json() { eprintln!("{line}") } else { println!("{line}") };
                say(format!("updating this PC from {} to {}…", update::VERSION, release.version));
                let exe = tokio::task::spawn_blocking(move || update::update_pc(&release)).await??;
                say(format!("updated {}", exe.display()));
                // The new copy puts its own version on the Switch.
                let mut next = std::process::Command::new(exe);
                next.args(["update", "--switch"]);
                if out.json() {
                    next.arg("--json");
                }
                if let Some(host) = &host {
                    next.args(["--host", host]);
                }
                let status = next.arg("--port").arg(port.to_string()).status()?;
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
    let pc = json!({ "version": update::VERSION, "latest": pc_latest.clone().unwrap_or_else(|| update::VERSION.to_string()) });
    let Ok(host) = config::host(host) else {
        out.note("no Switch paired, so only this PC was checked");
        out.done(json!({ "pc": pc, "switch": null }), String::new);
        return Ok(());
    };
    let client = Client::new(format!("{host}:{port}"), config::load_token()?).named(client::TERMINAL);
    let info = client.hello().await.with_context(|| format!("reaching the Switch at {host}"))?;
    let behind = update::switch_behind(&info.version) || info.protocol < proto::PROTOCOL_VERSION;
    if !behind {
        out.note(format!("the Switch has Kickstand {} already", info.version));
    } else if check {
        out.note(format!("the Switch has Kickstand {}: `kickstand update --switch` puts {} on it", info.version, update::VERSION));
    } else {
        out.note(format!("updating the Switch from {} to {} (its background service restarts, about 15 s)…", info.version, update::VERSION));
        for note in update::update_switch(&client).await? {
            out.note(note);
        }
    }
    let now = if behind && !check { update::VERSION.to_string() } else { info.version.clone() };
    out.done(json!({ "pc": pc, "switch": { "host": host, "was": info.version, "version": now, "behind": behind && check } }), String::new);
    Ok(())
}

/// `"x,y,w,h"` as the numbers.
/// Saves `samples` as a .wav (and its spectrogram), analyses it and prints the result with `note`.
fn save_audio(out: &out::Out, samples: &[i16], wav_out: Option<PathBuf>, reference: Option<PathBuf>, note: String) -> Result<()> {
    let wav_path = match wav_out {
        Some(p) => p,
        None => tools::default_capture_path("wav")?,
    };
    let (analysis, saved, _) = audio::format_audio_output(
        samples,
        audio::DEFAULT_SAMPLE_RATE,
        audio::DEFAULT_CHANNELS,
        &wav_path,
        reference.as_deref(),
        modalities::Modalities::IMAGE,
    )?;
    let spec_path = saved.with_extension("spectrogram.png");
    std::fs::write(&spec_path, audio::render_spectrogram_png(samples, audio::DEFAULT_SAMPLE_RATE, audio::DEFAULT_CHANNELS)?)?;
    let mut summary = format!("{}\nspectrogram: {}", analysis.summary_text(Some(&saved)).trim_end(), spec_path.display());
    if !note.is_empty() {
        summary = format!("{note}\n{summary}");
    }
    out.done(json!({ "path": saved, "spectrogram": spec_path, "analysis": analysis, "summary": summary }), || summary.clone());
    Ok(())
}

fn parse_region(text: &str) -> Result<(u32, u32, u32, u32)> {
    let n: Vec<u32> = text.split(',').map(|v| v.trim().parse()).collect::<Result<_, _>>().context("region is x,y,w,h")?;
    let [x, y, w, h] = n[..] else { anyhow::bail!("region is x,y,w,h") };
    Ok((x, y, w, h))
}

/// "1.23 MB/s" for `bytes` moved in `secs`.
fn mb_per_s(bytes: u64, secs: f64) -> String {
    format!("{:.2} MB/s", bytes as f64 / 1e6 / secs.max(1e-6))
}

/// `kickstand macro pause|resume|stop`.
async fn control_macro(out: &out::Out, client: &Client, action: proto::PlaybackAction) -> Result<()> {
    let was = client.playback(action).await?;
    let acted = macros::acted(&was, action);
    let by = was.app.as_deref().map(|a| macros::player(Some(a)));
    out.done(json!({ "acted": acted, "playing": was.playing, "paused": was.paused, "by": by }), || macros::control_text(&was, action));
    Ok(())
}

/// Saves a run's screenshot (a crash or hang) to `--out`, if both exist; where it went.
fn save_screenshot(out: Option<PathBuf>, jpeg: Option<Vec<u8>>) -> Result<Option<PathBuf>> {
    let (Some(out), Some(jpeg)) = (out, jpeg) else { return Ok(None) };
    std::fs::write(&out, jpeg)?;
    Ok(Some(out))
}

/// A report's text, with where its screenshot was saved.
fn with_screenshot(text: &str, saved: &Option<PathBuf>) -> String {
    match saved {
        Some(path) => format!("{text}\nscreenshot: {}", path.display()),
        None => text.to_string(),
    }
}

/// The CLI uses `path` outside the folders agents may use freely (access.rs) only when a person typed
/// the command: an agent's shell has no terminal on stdin.
fn typed_or_allowed(path: &str, need: access::Need, is_dir: bool) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() || access::Access::default().allowed(path, need) {
        return Ok(());
    }
    let folder = access::folder_of(path, is_dir);
    let flag = if need == access::Need::Write { " --write" } else { "" };
    anyhow::bail!(
        "{path} is outside the folders agents may {} without asking. Ask the user; they can allow it by typing \
         `kickstand allow {folder}{flag}` in a terminal",
        need.verb()
    )
}

#[cfg(test)]
mod tests;

fn describe_keep_awake(remaining_s: u64) -> String {
    match remaining_s {
        0 => "keep awake: off (the console's own dim and sleep timers apply)".into(),
        s => format!("keep awake: {}m {}s left", s / 60, s % 60),
    }
}
