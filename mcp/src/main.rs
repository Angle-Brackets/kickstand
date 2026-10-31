// SPDX-License-Identifier: GPL-3.0-only
//! kickstand: an MCP server (stdio) for the daemon on the Switch, a desktop app for setting it up and
//! following along, and a few CLI commands for using the daemon by hand.

mod capture;
mod client;
mod config;
mod crash;
mod desktop;
mod launch;
mod link;
mod memory;
mod mock;
mod nxlink;
mod pairing;
mod project;
mod symbolize;
mod tools;
mod watch;
mod watchdog;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server on stdio (what AI apps start; the default when stdin is a pipe).
    Serve,
    /// Open the desktop app (the default otherwise): find the Switch, pair, link an AI app, and see what
    /// the agent is doing.
    Desktop,
    /// Create the shared token if there isn't one, and print it and where the console needs it.
    Token,
    /// Add Kickstand to Claude Code or Claude Desktop ("code" or "desktop"); without one, show where it's
    /// linked.
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
    /// Check the daemon answers.
    Ping,
    /// List a directory on the SD card.
    Ls { path: String },
    /// Print a file from the SD card.
    Cat { path: String },
    /// Copy a file from the SD card to this PC.
    Get { path: String, local: PathBuf },
    /// Upload a file to the SD card.
    Put { local: PathBuf, path: String },
    /// Delete a file or directory on the SD card (inside the writable folders only).
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
    /// The running application, if any, and whether it's hbloader (hbmenu or an NRO).
    App,
    /// Where the console is: uptime, unlock presses after a reboot, keep-awake, application, applet.
    Status,
    /// Close the running application (terminated without saving), or with --applet, the Album (with HOME).
    CloseApp {
        #[arg(long)]
        applet: bool,
    },
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
    /// Build, deploy, run and watch a project from ~/.config/kickstand/projects.json, e.g. `{"paperboat":
    /// {"dir": "...", "build": "cmake --build build-switch --target Paperboat_nro", "nro":
    /// "build-switch/paperboat.nro", "elf": "build-switch/Paperboat.elf", "dest":
    /// "/switch/paperboat/paperboat.nro"}}` (nro and elf relative to dir; optional "args", "module").
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
    /// Record the screen as a looping GIF from repeated screenshots.
    Gif {
        local: PathBuf,
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
    },
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
    let command = cli.command.take().unwrap_or(if stdin_is_pipe() { Command::Serve } else { Command::Desktop });
    if let Command::Desktop = command {
        return desktop::run();
    }
    run(cli, command)
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

    match command {
        Command::Symbolize { elf, text, module } => {
            let symbols = symbolize::Symbolizer::new(&elf, module.as_deref())?;
            let text = match text {
                Some(t) => t,
                None => std::io::read_to_string(std::io::stdin())?,
            };
            print!("{}", symbols.annotate(&text));
            return Ok(());
        }
        Command::Token => {
            let (token, path) = config::ensure_token()?;
            println!("token: {token}");
            println!("saved in {}", path.display());
            println!("`kickstand pair <code>` sends it to a Switch; by hand, add it as a line of {} on the SD card", proto::path::TOKEN_PATH);
            return Ok(());
        }
        Command::Mock { root, bind, pair_code } => {
            let tokens = config::load_token().ok().into_iter().collect();
            let listener = tokio::net::TcpListener::bind((bind.as_str(), cli.port)).await?;
            let probes = tokio::net::UdpSocket::bind((bind.as_str(), proto::discovery::PORT)).await?;
            eprintln!("mock daemon serving {} on {} (discovery on UDP {})", root.display(), listener.local_addr()?, probes.local_addr()?);
            tokio::spawn(mock::answer_probes(probes, cli.port, pair_code.is_some()));
            let console = mock::Console::new(root, tokens, pair_code);
            return mock::serve_console(listener, Arc::new(console)).await;
        }
        Command::Link { app, remove } => {
            let Some(app) = app else {
                for app in link::App::ALL {
                    let state = match link::state(app) {
                        link::State::Missing => "not installed".to_string(),
                        link::State::NotLinked => "not linked".to_string(),
                        link::State::Linked { command } => format!("linked ({command})"),
                    };
                    println!("{}: {state}", app.name());
                }
                return Ok(());
            };
            let app = match app.to_ascii_lowercase().as_str() {
                "code" | "claude-code" => link::App::ClaudeCode,
                "desktop" | "claude-desktop" => link::App::ClaudeDesktop,
                other => anyhow::bail!("{other:?}: say \"code\" (Claude Code) or \"desktop\" (Claude Desktop)"),
            };
            println!("{}", if remove { link::unlink(app)? } else { link::link(app)? });
            return Ok(());
        }
        Command::Discover { to } => {
            let to = match to {
                Some(host) => Some(format!("{host}:{}", proto::discovery::PORT).parse().context("--to takes an IPv4 address")?),
                None => None,
            };
            let found = pairing::discover(pairing::DISCOVER_WAIT, to).await?;
            if found.is_empty() {
                println!("no Switch running Kickstand answered (is it on the same network as this PC?)");
            }
            for f in found {
                let a = &f.announce;
                let kind = match a.kind {
                    proto::DaemonKind::Sysmodule => "background service",
                    proto::DaemonKind::Nro => "on-screen app",
                };
                let pairing = if a.pairing { ", showing a pairing code" } else { "" };
                println!("{}: Kickstand {} ({kind}, protocol {}){pairing}", f.describe(), a.daemon_version, a.protocol);
            }
            return Ok(());
        }
        Command::Pair { code } => {
            let host = pairing::pick(cli.host.as_deref(), cli.port).await?;
            let info = pairing::pair(&host, cli.port, &code).await?;
            println!("paired with the Switch at {host} (Kickstand {}); this PC uses it from now on", info.version);
            return Ok(());
        }
        _ => {}
    }

    let host = config::host(cli.host)?;
    let client = Arc::new(Client::new(format!("{host}:{}", cli.port), config::load_token()?));

    match command {
        Command::Desktop => unreachable!("opened before the runtime starts"),
        Command::Serve => {
            let service = tools::Tools::new(client).serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        Command::Ping => {
            let info = client.hello().await?;
            let (rtt, uptime) = client.ping().await?;
            println!("daemon {} ({:?}, protocol {}): rtt {:.1} ms, uptime {}s", info.version, info.kind, info.protocol, rtt.as_secs_f64() * 1e3, uptime.as_secs());
        }
        Command::Ls { path } => {
            let (entries, truncated) = client.list_dir(&check_path(&path, false)?).await?;
            for e in entries {
                if e.is_dir { println!("{}/", e.name) } else { println!("{}\t{}", e.name, e.size) }
            }
            if truncated {
                println!("... (truncated)");
            }
        }
        Command::Cat { path } => {
            let path = check_path(&path, false)?;
            let size = client.stat(&path).await?.size;
            let (data, _) = client.read(&path, 0, size).await?;
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }
        Command::Get { path, local } => {
            let path = check_path(&path, false)?;
            let size = client.stat(&path).await?.size;
            let start = std::time::Instant::now();
            let (data, _) = client.read(&path, 0, size).await?;
            let secs = start.elapsed().as_secs_f64();
            std::fs::write(&local, &data).with_context(|| format!("writing {}", local.display()))?;
            println!("{} bytes in {secs:.1}s ({:.2} MB/s) -> {}", data.len(), data.len() as f64 / 1e6 / secs.max(1e-6), local.display());
        }
        Command::Put { local, path } => {
            let path = check_path(&path, true)?;
            let data = std::fs::read(&local).with_context(|| format!("reading {}", local.display()))?;
            let stats = client.upload(&path, &data).await?;
            let secs = stats.elapsed.as_secs_f64();
            println!("{} bytes in {secs:.1}s ({:.2} MB/s)", stats.size, stats.size as f64 / 1e6 / secs.max(1e-6));
        }
        Command::Rm { path, recursive } => {
            let path = check_path(&path, true)?;
            let was_dir = client.delete(&path, recursive).await?;
            println!("deleted {path}{}", if was_dir { "/" } else { "" });
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
                            println!("{note}");
                        }
                    }
                    client.sysmodule(proto::Request::SysmoduleStatus).await?
                }
            };
            println!("{status:?}");
        }
        Command::Reboot { target, yes } => {
            let entries = client.hekate_entries().await?;
            println!("hekate entries:");
            for (i, e) in entries.iter().enumerate() {
                println!("  {}. {e}", i + 1);
            }
            let Some(target) = target else { return Ok(()) };
            let target = client::parse_hekate_target(&target, &entries)?;
            let what = tools::describe_target(target, &entries);
            if !yes {
                println!("would reboot into hekate: {what} (add --yes to do it)");
                return Ok(());
            }
            client.reboot_to_hekate(target).await?;
            println!("rebooting into hekate: {what}");
        }
        Command::IramPeek { offset, compare } => {
            let offset = match offset.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16)?,
                None => offset.parse()?,
            };
            let page = client.iram_peek(offset).await?;
            for (i, row) in page.chunks(16).take(8).enumerate() {
                let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
                println!("{:#07x}: {}", 0x4000_0000u32 + offset + i as u32 * 16, hex.join(" "));
            }
            if let Some(file) = compare {
                let data = std::fs::read(&file)?;
                let start = (offset as usize).checked_sub(0x10000).context("compare needs an offset of at least 0x10000")?;
                let want = data.get(start..(start + page.len()).min(data.len())).unwrap_or(&[]);
                let same = want.iter().zip(&page).filter(|(a, b)| a == b).count();
                println!("{same} of {} bytes match {} at {start:#x}", want.len(), file.display());
            }
        }
        Command::Press { buttons, times, hold_ms } => {
            let (buttons, times) = match buttons.as_str() {
                "unlock" => (vec![proto::Button::B], 3),
                spec => (client::parse_buttons(spec)?, times),
            };
            client.press(buttons, times, hold_ms).await?;
            println!("pressed");
        }
        Command::Input { steps } => {
            client.input(client::parse_steps(&steps)?).await?;
            println!("played");
        }
        Command::Status => {
            if let Some(fatal) = client.fatal().await? {
                println!("{fatal}");
            }
            println!("{}", client::describe_status(&client.status().await?));
        }
        Command::App => {
            match client.app_status().await? {
                Some(app) => println!("application: {}", client::describe_app(&app)),
                None => println!("application: none"),
            }
            match client.applet_status().await? {
                Some(a) => println!("applet: the Album, pid {}{}", a.pid, if a.hbl == Some(true) { ", hbloader" } else { "" }),
                None => println!("applet: the Album isn't open"),
            }
        }
        Command::CloseApp { applet } => {
            let mode = if applet { launch::Mode::Applet } else { launch::Mode::Full };
            match launch::close_running(&client, mode).await? {
                Some((app, how)) => println!("closed {} ({})", client::describe_app(&app), launch::describe_closed(how)),
                None if applet => println!("the Album wasn't open"),
                None => println!("no application was running"),
            }
        }
        Command::Apps { max } => {
            for (i, app) in client.list_apps(max).await?.iter().enumerate() {
                println!("{i:>3}  {:016x}  {}", app.id, app.name);
            }
        }
        Command::Watch { http_port, bind, fps } => watch::serve(client.clone(), &bind, http_port, fps).await?,
        Command::Launch { title, user } => {
            let (app, notes) = launch::launch_title(&client, &title, user).await?;
            for note in &notes {
                println!("{note}");
            }
            println!("{} ({:016x}) is running", app.name, app.id);
        }
        Command::Hbmenu { full } => {
            let opened = launch::open_hbmenu(&client, if full { launch::Mode::Full } else { launch::Mode::Applet }).await?;
            for note in &opened.notes {
                println!("{note}");
            }
            match opened.host {
                Some(host) => println!("hbmenu is open with full memory (host title {:016x} {})", host.id, host.name),
                None => println!("hbmenu is open in applet mode"),
            }
        }
        Command::Crashes { limit } => {
            for e in crash::list(&client, limit).await? {
                println!("{}\n    {}", e.path, e.headline);
            }
        }
        Command::Crash { name, elf, module, screenshot } => {
            let fetched = crash::fetch(&client, &name).await?;
            let symbols = elf.map(|e| symbolize::Symbolizer::new(&e, module.as_deref())).transpose()?;
            print!("{}", crash::describe(&fetched, symbols.as_ref())?);
            if let (Some(path), Some(jpg)) = (screenshot, &fetched.jpg) {
                std::fs::write(&path, jpg)?;
                println!("screenshot saved to {}", path.display());
            }
        }
        Command::Wait { until, timeout, region, out } => {
            let until = if until == "still" { capture::Until::Still } else { capture::Until::Change };
            let region = match region {
                Some(r) => {
                    let n: Vec<u32> = r.split(',').map(|v| v.trim().parse()).collect::<Result<_, _>>().context("region is x,y,w,h")?;
                    let [x, y, w, h] = n[..] else { anyhow::bail!("region is x,y,w,h") };
                    Some((x, y, w, h))
                }
                None => None,
            };
            let waited = capture::wait(&client, until, region, std::time::Duration::from_secs_f64(timeout.clamp(0.0, capture::MAX_WAIT_SECONDS))).await?;
            println!("{} after {:.1} s, {} captures", if waited.met { "met" } else { "timed out" }, waited.elapsed.as_secs_f64(), waited.captures);
            if let Some(out) = out {
                std::fs::write(&out, &waited.jpeg)?;
            }
        }
        Command::Test { project, no_build, seconds, quiet, out, args } => {
            let settings = watchdog::Settings {
                max: std::time::Duration::from_secs(seconds),
                quiet: std::time::Duration::from_secs(quiet.max(1)),
                reboot_on_fatal: true,
            };
            let output = Arc::new(std::sync::Mutex::new(launch::OutputLog::default()));
            let args = (!args.is_empty()).then_some(args);
            let run = project::test(&client, &project::get(&project)?, no_build, args, &output, &settings).await?;
            println!("{}", run.text);
            if let (Some(out), Some(jpeg)) = (out, run.screenshot) {
                std::fs::write(&out, jpeg)?;
                println!("screenshot: {}", out.display());
            }
        }
        Command::Projects => {
            for (name, p) in project::load_all()? {
                println!("{name}: {} (build: {}; {} -> {})", p.dir.display(), p.build.as_deref().unwrap_or("none"), p.nro.display(), p.dest);
            }
        }
        Command::Supervise { seconds, quiet, elf, no_reboot, out } => {
            let settings = watchdog::Settings {
                max: std::time::Duration::from_secs(seconds),
                quiet: std::time::Duration::from_secs(quiet.max(1)),
                reboot_on_fatal: !no_reboot,
            };
            let report = watchdog::supervise(&client, None, &memory::Elf::load(elf.as_deref(), None)?, &settings).await?;
            println!("{}", report.text);
            if let (Some(out), Some(jpeg)) = (out, report.screenshot) {
                std::fs::write(&out, jpeg)?;
                println!("screenshot: {}", out.display());
            }
        }
        Command::Threads { elf, module } => {
            print!("{}", memory::threads(&client, &memory::Elf::load(elf.as_deref(), module.as_deref())?).await?)
        }
        Command::Memory { elf } => print!("{}", memory::map(&client, &memory::Elf::load(elf.as_deref(), None)?).await?),
        Command::Peek { address, len, format, elf, module } => {
            let elf = memory::Elf::load(elf.as_deref(), module.as_deref())?;
            print!("{}", memory::peek(&client, &elf, &address, len, memory::Format::parse(&format)?).await?);
        }
        Command::Poke { address, value, format, elf, module } => {
            let elf = memory::Elf::load(elf.as_deref(), module.as_deref())?;
            println!("{}", memory::poke(&client, &elf, &address, &value, memory::Format::parse(&format)?).await?);
        }
        Command::Run { nro, applet, dest, elf, module, args } => {
            let program = match nro.strip_prefix("sdmc:") {
                Some(path) => launch::Program::Sd(path.to_string()),
                None => {
                    let name = std::path::Path::new(&nro).file_name().context("NRO has no file name")?.to_string_lossy();
                    let dest = dest.unwrap_or_else(|| format!("/switch/kickstand/{name}"));
                    launch::Program::Local { nro: std::fs::read(&nro).with_context(|| format!("reading {nro}"))?, dest }
                }
            };
            let symbols = elf.map(|e| symbolize::Symbolizer::new(&e, module.as_deref()).map(symbolize::Symbolizer::shared)).transpose()?;
            let mode = if applet { launch::Mode::Applet } else { launch::Mode::Full };
            let started = launch::run_nro(&client, program, args, mode).await?;
            for note in &started.notes {
                eprintln!("{note}");
            }
            if let Some(output) = started.output {
                use std::io::Write as _;
                let mut lines = launch::Lines::new(output, symbols);
                while let Some(line) = lines.next().await {
                    print!("{line}");
                    let _ = std::io::stdout().flush();
                }
                eprintln!("(the NRO closed its stdout)");
            }
        }
        Command::Screenshot { local } => {
            let jpeg = client.screenshot().await?;
            std::fs::write(&local, &jpeg)?;
            println!("saved {} ({} bytes)", local.display(), jpeg.len());
        }
        Command::Gif { local, seconds, fps, width } => {
            let shots = capture::record(&client, seconds, fps).await?;
            let gif = tokio::task::spawn_blocking(move || capture::gif(&shots, width).map(|g| (g, shots.len())));
            let (gif, frames) = gif.await??;
            std::fs::write(&local, &gif)?;
            println!("saved {} ({frames} frames, {} bytes)", local.display(), gif.len());
        }
        Command::IndicatorAt { x, y } => client.indicator_position(x, y).await?,
        Command::KeepAwake { minutes } => println!("{}", describe_keep_awake(client.keep_awake(minutes).await?)),
        Command::Resources => print!("{}", client::describe_resources(&client.resources().await?)),
        Command::Exit => {
            client.exit().await?;
            println!("the NRO daemon is exiting");
        }
        Command::Token | Command::Mock { .. } | Command::Symbolize { .. } | Command::Discover { .. } | Command::Pair { .. } | Command::Link { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

fn describe_keep_awake(remaining_s: u64) -> String {
    match remaining_s {
        0 => "keep awake: off (the console's own dim and sleep timers apply)".into(),
        s => format!("keep awake: {}m {}s left", s / 60, s % 60),
    }
}
