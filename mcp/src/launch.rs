// SPDX-License-Identifier: GPL-3.0-only
//! Opening hbmenu with nobody at the console, in one of two places ([`Mode`]):
//! - **Applet mode**: the home menu's Album button, which Atmosphère runs hbloader in place of. Quick and
//!   consistent (no game involved; a running game stays suspended), but homebrew gets a few hundred MB of
//!   memory. For starting the Kickstand NRO and small tools only: games (the projects in projects.json) never run
//!   here (the user's rule), and [`run_nro`] refuses them.
//! - **Full memory** (what [`run_nro`] uses by default): Atmosphère starts hbloader, with the title's
//!   memory, in place of a title launched from the home menu. A person holds R for that; the virtual
//!   controller's R wasn't seen reliably, so the daemon instead arms an entry in Atmosphère's override config
//!   for that one title just before pressing A and disarms it once the process exists
//!   ([`Client::hbl_override`]). Not every title works (ones that check online rights, game cards that
//!   aren't inserted), and only the home menu's row can do it, not "All Software". So the first time, try
//!   each tile of the row in order; remember the one that worked (the host title, in
//!   `~/.config/kickstand/host-title`) and go straight to it afterwards.
//!
//! Running an NRO then goes through hbmenu's NetLoader ([`run_nro`]): an NRO from this PC is sent as is;
//! one already on the SD card is opened by sending the Kickstand NRO with `--open <path>`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use proto::{App, AppEntry, Button, DaemonKind, HOME_MENU_TILES, InputStep};
use tokio::net::TcpStream;

use crate::client::{Client, check_path};
use crate::{config, nxlink};

/// The pid of the hbloader process last seen showing hbmenu (0: none known): set when [`open_hbmenu`]
/// opens it or an NRO [`run_nro`] started closes its stdout (it exited, back to hbmenu), cleared when an NRO
/// is sent. Lets [`run_nro`] press Y straight away instead of reopening hbmenu.
static HBMENU_PID: AtomicU64 = AtomicU64::new(0);

/// Shortest wait for the home menu to come back after an application closes.
const AFTER_CLOSE: Duration = Duration::from_millis(1500);
/// How long to look for a new application process after the last input.
const PROCESS_WAIT: Duration = Duration::from_secs(5);
/// How long the Album gets to exit after HOME.
const EXIT_WAIT: Duration = Duration::from_secs(4);

pub struct Opened {
    /// The title hbmenu runs in place of, in full-memory mode.
    pub host: Option<AppEntry>,
    /// What was tried on the way, for the agent to relay.
    pub notes: Vec<String>,
}

fn step(buttons: &[Button], ms: u32) -> InputStep {
    InputStep { buttons: buttons.to_vec(), ms, ..Default::default() }
}

/// Where hbmenu runs (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Applet,
    Full,
}

impl Mode {
    /// `name`, or `default` if none was given.
    pub fn parse(name: Option<&str>, default: Mode) -> Result<Self> {
        let Some(name) = name else { return Ok(default) };
        match name {
            "applet" => Ok(Mode::Applet),
            "full" => Ok(Mode::Full),
            other => bail!("mode is \"applet\" or \"full\", not {other:?}"),
        }
    }
}

/// The process hbmenu runs in for `mode`: the Album's, or the application's.
async fn status(client: &Client, mode: Mode) -> Result<Option<App>> {
    match mode {
        Mode::Applet => client.applet_status().await,
        Mode::Full => client.app_status().await,
    }
}

/// Opens hbmenu in `mode`. In full-memory mode, closes whatever application runs first and fails with what
/// it tried if no title on the row works.
pub async fn open_hbmenu(client: &Client, mode: Mode) -> Result<Opened> {
    match mode {
        Mode::Applet => open_applet(client, false).await,
        Mode::Full => open_full(client).await,
    }
}

/// How long the Album gets to show up after A.
const ALBUM_WAIT: Duration = Duration::from_secs(8);

/// `at_home`: the Album was just closed, which leaves the home menu showing with its first tile selected.
/// HOME then would resume a suspended game instead.
async fn open_applet(client: &Client, mut at_home: bool) -> Result<Opened> {
    let mut notes = Vec::new();
    if let Some(app) = client.applet_status().await? {
        if app.hbl == Some(true) {
            notes.push("hbmenu (or an NRO) was already open in applet mode".into());
            return Ok(Opened { host: None, notes });
        }
        close(client, Mode::Applet, &app).await?;
        notes.push("closed the Album (it was showing pictures, not hbmenu)".into());
        at_home = true;
    }
    // HOME selects the first tile; Down moves to the leftmost button under the row, and the Album is the
    // fourth.
    let mut steps = if at_home { vec![] } else { vec![step(&[Button::Home], 100), step(&[], 1500)] };
    steps.extend([step(&[Button::Down], 100), step(&[], 400)]);
    for _ in 0..3 {
        steps.extend([step(&[Button::Right], 80), step(&[], 200)]);
    }
    steps.push(step(&[Button::A], 100));
    client.input(steps).await?;
    let start = tokio::time::Instant::now();
    loop {
        match client.applet_status().await? {
            Some(app) if app.hbl == Some(true) => {
                HBMENU_PID.store(app.pid, Ordering::Relaxed);
                notes.push("hbmenu opened in applet mode (the Album)".into());
                return Ok(Opened { host: None, notes });
            }
            Some(_) => bail!("the Album opened without hbloader: Atmosphère's hbl_config doesn't override it here"),
            None if start.elapsed() > ALBUM_WAIT => {
                bail!("the Album didn't open: the home menu may not have been showing; check with screenshot")
            }
            None => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Opens hbmenu with full memory through a title.
async fn open_full(client: &Client) -> Result<Opened> {
    let mut notes = Vec::new();
    if let Some(app) = client.app_status().await? {
        if app.hbl == Some(true) {
            let host = AppEntry { id: app.program_id, name: String::new() };
            notes.push("hbloader was already running (hbmenu or an NRO)".into());
            return Ok(Opened { host: Some(host), notes });
        }
        close(client, Mode::Full, &app).await?;
        notes.push(format!("closed the running title {:016x}", app.program_id));
    }

    let apps = client.list_apps(HOME_MENU_TILES as u16).await?;
    if apps.is_empty() {
        bail!("no titles are installed; one is needed to open hbmenu with full memory");
    }
    // The saved host title first, then the rest of the row in order.
    let saved = config::host_title();
    let mut order: Vec<usize> = (0..apps.len()).collect();
    if let Some(i) = saved.and_then(|id| apps.iter().position(|a| a.id == id)) {
        order.remove(i);
        order.insert(0, i);
    } else if let Some(id) = saved {
        notes.push(format!("the saved host title {id:016x} isn't on the home menu's row any more"));
    }

    for tile in order {
        let expected = &apps[tile];
        let label = if expected.name.is_empty() { format!("{:016x}", expected.id) } else { expected.name.clone() };
        match try_tile(client, tile, expected.id, &mut notes).await? {
            Some(app) if app.hbl == Some(true) => {
                HBMENU_PID.store(app.pid, Ordering::Relaxed);
                let host = apps.iter().find(|a| a.id == app.program_id).cloned().unwrap_or(AppEntry { id: app.program_id, name: String::new() });
                config::save_host_title(host.id, &host.name)?;
                notes.push(format!("hbmenu opened through {label} (tile {tile})"));
                return Ok(Opened { host: Some(host), notes });
            }
            Some(app) => {
                let how = if app.hbl.is_none() { "Atmosphère didn't say whether it was hbloader" } else { "it started as the game" };
                notes.push(format!("{label} (tile {tile}): {how}"));
                close(client, Mode::Full, &app).await?;
            }
            None => {
                notes.push(format!("{label} (tile {tile}): nothing started (it needs a card, rights or an online check)"));
                dismiss_dialogs(client).await?;
            }
        }
    }
    bail!("no title on the home menu's row opened hbmenu:\n- {}", notes.join("\n- "))
}

/// Selects tile `tile` of the home menu's row: HOME on the home menu selects the first tile.
async fn select_tile(client: &Client, tile: usize) -> Result<()> {
    let mut steps = vec![step(&[Button::Home], 100), step(&[], 1200)];
    for _ in 0..tile {
        steps.extend([step(&[Button::Right], 80), step(&[], 150)]);
    }
    client.input(steps).await
}

/// Starts tile `tile` (title `id`) of the home menu's row as hbloader; returns the application that
/// started, if any.
async fn try_tile(client: &Client, tile: usize, id: u64, notes: &mut Vec<String>) -> Result<Option<App>> {
    if !client.hbl_override(id).await? {
        notes.push("the sysmodule predates protocol 20, so R is held instead (unreliable): update it".into());
        return try_tile_holding_r(client, tile).await;
    }
    let started = start_tile(client, tile).await;
    // pm reads the config when it creates the process, so the title can go back to starting as itself.
    let disarmed = client.hbl_override(0).await.context("disarming the hbl override");
    let app = started?;
    disarmed?;
    Ok(app)
}

/// Starts tile `tile` of the home menu's row; returns the application that started, if any. A title that
/// asks who is playing only starts once that's answered, and the picker starts on the first user. Right
/// after boot the picker can take a few seconds to appear and swallow a press, so press again while
/// nothing has started.
async fn start_tile(client: &Client, tile: usize) -> Result<Option<App>> {
    select_tile(client, tile).await?;
    for _ in 0..3 {
        client.input(vec![step(&[Button::A], 100), step(&[], 2500)]).await?;
        if let Some(app) = client.app_status().await? {
            return Ok(Some(app));
        }
    }
    wait_for_app(client).await
}

/// [`try_tile`] for a daemon without [`Client::hbl_override`]: R held through the launch.
async fn try_tile_holding_r(client: &Client, tile: usize) -> Result<Option<App>> {
    select_tile(client, tile).await?;
    // Holding R long enough to cover the process starting, and through the user picker.
    let launch = vec![step(&[Button::R], 300), step(&[Button::R, Button::A], 150), step(&[Button::R], 6000)];
    for _ in 0..2 {
        client.input(launch.clone()).await?;
        if let Some(app) = client.app_status().await? {
            return Ok(Some(app));
        }
    }
    client.input(launch).await?;
    wait_for_app(client).await
}

/// Starts an installed title from the home menu's row, as the user would: `query` is its id (hex) or part
/// of its name; `user` picks the account if the title asks who is playing (0 = the first). Closes the
/// running application first.
pub async fn launch_title(client: &Client, query: &str, user: usize) -> Result<(AppEntry, Vec<String>)> {
    let apps = client.list_apps(HOME_MENU_TILES as u16).await?;
    let id = u64::from_str_radix(query.trim_start_matches("0x"), 16).ok().filter(|_| query.len() >= 16);
    let lower = query.to_lowercase();
    let matches: Vec<usize> = (0..apps.len())
        .filter(|&i| Some(apps[i].id) == id || (!lower.is_empty() && apps[i].name.to_lowercase().contains(&lower)))
        .collect();
    let tile = match matches.as_slice() {
        [tile] => *tile,
        [] => bail!(
            "no title on the home menu's row matches {query:?} (only the first {HOME_MENU_TILES} can be started this way): {}",
            apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
        many => bail!("{query:?} matches several titles: {}", many.iter().map(|&i| apps[i].name.as_str()).collect::<Vec<_>>().join(", ")),
    };
    let target = apps[tile].clone();
    let mut notes = Vec::new();
    if let Some(app) = client.app_status().await? {
        if app.program_id == target.id && app.hbl == Some(false) {
            notes.push(format!("{} was already running; switch to it with HOME then A", target.name));
            return Ok((target, notes));
        }
        close(client, Mode::Full, &app).await?;
        notes.push(format!("closed the running application {:016x}", app.program_id));
    }
    select_tile(client, tile).await?;
    client.input(vec![step(&[Button::A], 100), step(&[], 2500)]).await?;
    if client.app_status().await?.is_none() {
        // Who is playing? The picker starts on the first user.
        let mut steps = Vec::new();
        for _ in 0..user {
            steps.extend([step(&[Button::Right], 80), step(&[], 150)]);
        }
        steps.push(step(&[Button::A], 100));
        client.input(steps).await?;
    }
    match wait_for_app(client).await? {
        Some(app) if app.program_id == target.id => {
            notes.push(format!("started {} (pid {})", target.name, app.pid));
            Ok((target, notes))
        }
        Some(app) => bail!("expected {} to start, but {:016x} did", target.name, app.program_id),
        None => {
            dismiss_dialogs(client).await?;
            bail!("{} didn't start (it may need its game card, rights or an online check)", target.name)
        }
    }
}

/// Backs out of what a title that didn't start leaves behind. Nintendo Switch Online, for one, chains
/// "Sign in to your Nintendo Account again", an error (2155-8007) a few seconds later, then the user
/// picker. B closes each, and does nothing once the home menu is back.
async fn dismiss_dialogs(client: &Client) -> Result<()> {
    let steps = (0..4).flat_map(|_| [step(&[Button::B], 100), step(&[], 3500)]).collect();
    client.input(steps).await
}

async fn wait_for_app(client: &Client) -> Result<Option<App>> {
    let start = tokio::time::Instant::now();
    loop {
        if let Some(app) = client.app_status().await? {
            return Ok(Some(app));
        }
        if start.elapsed() > PROCESS_WAIT {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// How a process was closed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Closed {
    /// It exited when HOME was pressed (the Album).
    Exited,
    /// It was terminated (an application).
    Terminated,
}

/// Closes `app` and waits for the home menu to take over again. The Album is closed with HOME, which makes
/// the system ask it to exit: terminating it behind the home menu's back crashes the home menu (2144-0001,
/// twice; see `context/AGENT_LOG.md`), and hbmenu has no exit button. An application (a game, or hbloader
/// with full memory) is terminated.
pub async fn close(client: &Client, mode: Mode, app: &App) -> Result<Closed> {
    if mode == Mode::Applet {
        client.input(vec![step(&[Button::Home], 100)]).await?;
        if !gone(client, mode, app, EXIT_WAIT).await? {
            bail!(
                "the Album (pid {}) didn't exit after HOME; it's left running, since terminating it crashes the home menu",
                app.pid
            );
        }
        HBMENU_PID.store(0, Ordering::Relaxed);
        tokio::time::sleep(AFTER_CLOSE).await;
        return Ok(Closed::Exited);
    }
    client.close_app().await?;
    if !gone(client, mode, app, PROCESS_WAIT).await? {
        bail!("application {:016x} didn't close", app.program_id);
    }
    tokio::time::sleep(AFTER_CLOSE).await;
    Ok(Closed::Terminated)
}

/// Closes whatever runs in `mode`'s place (the Album, or the application), if anything, as [`close`] does.
pub async fn close_running(client: &Client, mode: Mode) -> Result<Option<(App, Closed)>> {
    let Some(app) = status(client, mode).await? else { return Ok(None) };
    let closed = close(client, mode, &app).await?;
    Ok(Some((app, closed)))
}

pub fn describe_closed(how: Closed) -> &'static str {
    match how {
        Closed::Exited => "it exited on HOME",
        Closed::Terminated => "terminated",
    }
}

/// Waits up to `wait` for `app`'s process to end; whether it did.
async fn gone(client: &Client, mode: Mode, app: &App, wait: Duration) -> Result<bool> {
    let start = tokio::time::Instant::now();
    while status(client, mode).await?.is_some_and(|now| now.pid == app.pid) {
        if start.elapsed() > wait {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(true)
}

/// The Kickstand NRO, whose launcher mode opens an NRO already on the SD card.
const LAUNCHER: &str = "/switch/kickstand/kickstand.nro";
/// How long NetLoader gets to start listening after Y.
const NETLOADER_WAIT: Duration = Duration::from_secs(15);
/// The same when hbmenu should already be up.
const NETLOADER_FAST_WAIT: Duration = Duration::from_secs(5);
/// How long the NRO gets to connect its stdout after it's sent.
const OUTPUT_WAIT: Duration = Duration::from_secs(10);

pub enum Program {
    /// An NRO on this PC, sent to `dest` on the SD card (inside the write roots, under `/switch/`).
    Local { nro: Vec<u8>, dest: String },
    /// An NRO already on the SD card.
    Sd(String),
}

pub struct Started {
    pub notes: Vec<String>,
    /// The hbloader process running the NRO; it shows hbmenu again once the NRO exits.
    pub pid: u64,
    /// The NRO's stdout, if it connected one (libnx's nxlink stdio, or the `nxlink` crate).
    pub output: Option<TcpStream>,
}

/// Starts an NRO with `args` from a fresh hbmenu in `mode`: an NRO already running there is closed first
/// (and in full-memory mode, any game).
pub async fn run_nro(client: &Client, program: Program, args: Vec<String>, mode: Mode) -> Result<Started> {
    let target = match &program {
        Program::Local { dest, .. } => dest,
        Program::Sd(path) => path,
    };
    if mode == Mode::Applet && is_game(target, &crate::project::load_all().unwrap_or_default()) {
        bail!("{target} is a game: it runs with full memory only (mode \"full\"), never in applet mode");
    }
    let (nro, dest, args) = match program {
        Program::Local { nro, dest } => {
            let dest = check_path(&dest, true)?;
            let parent = &dest[..dest.rfind('/').unwrap_or(0)];
            if !client.stat(parent).await.is_ok_and(|e| e.is_dir) {
                bail!("{parent} doesn't exist; hbmenu won't create it (make it with upload first)");
            }
            let dest = dest.strip_prefix("/switch/").context("NetLoader can only write under /switch/")?.to_string();
            (nro, dest, args)
        }
        Program::Sd(path) => {
            let path = check_path(&path, false)?;
            if !path.to_ascii_lowercase().ends_with(".nro") || client.stat(&path).await.is_err() {
                bail!("{path} isn't an NRO on the SD card");
            }
            let (launcher, _) = client.read(LAUNCHER, 0, u64::MAX).await.context("reading the Kickstand NRO to launch it")?;
            let dest = LAUNCHER.trim_start_matches("/switch/").to_string();
            (launcher, dest, ["--open".to_string(), path].into_iter().chain(args).collect())
        }
    };

    let mut notes = Vec::new();
    let output = nxlink::OutputListener::bind().await?;
    // hbmenu can't be told apart from an NRO, so only a process known to show hbmenu is used as is.
    let current = status(client, mode).await?;
    let mut sock = None;
    if let Some(app) = &current
        && app.hbl == Some(true)
        && app.pid == HBMENU_PID.load(Ordering::Relaxed)
    {
        match start_netloader(client, NETLOADER_FAST_WAIT).await {
            Ok(s) => {
                notes.push("hbmenu was already open".into());
                sock = Some(s);
            }
            Err(_) => notes.push("hbmenu didn't answer as expected; reopening it".into()),
        }
    }
    let sock = match sock {
        Some(sock) => sock,
        None => {
            let mut closed = false;
            if let Some(app) = status(client, mode).await?
                && app.hbl == Some(true)
            {
                close(client, mode, &app).await?;
                notes.push("closed the running hbloader (hbmenu or an NRO)".into());
                closed = true;
            }
            let opened = match mode {
                Mode::Applet => open_applet(client, closed).await?,
                Mode::Full => open_full(client).await?,
            };
            notes.extend(opened.notes);
            start_netloader(client, NETLOADER_WAIT).await?
        }
    };
    HBMENU_PID.store(0, Ordering::Relaxed);
    nxlink::send(sock, &nro, &dest, &args).await?;
    notes.push(format!("sent {} bytes as sdmc:/switch/{dest}", nro.len()));
    let pid = status(client, mode).await?.map_or(0, |a| a.pid);
    let output = output.accept(OUTPUT_WAIT).await?;
    if output.is_none() {
        notes.push("the NRO didn't connect its stdout (it may not use nxlink stdio)".into());
    }
    Ok(Started { notes, pid, output })
}

/// How long the Kickstand NRO gets to stop the sysmodule and serve the port itself.
const NRO_DAEMON_WAIT: Duration = Duration::from_secs(20);

/// Restarts the sysmodule without a reboot, so a newly installed `exefs.nsp` runs: the Kickstand NRO,
/// started in applet mode, stops the sysmodule as it starts and starts it again on handover, then HOME
/// closes the Album. `nro` (a newer Kickstand NRO) replaces the one on the SD card on the way, if given.
pub async fn restart_sysmodule(client: &Client, nro: Option<Vec<u8>>) -> Result<Vec<String>> {
    let nro = match nro {
        Some(nro) => nro,
        None => client.read(LAUNCHER, 0, u64::MAX).await.context("reading the Kickstand NRO")?.0,
    };
    let started = run_nro(client, Program::Local { nro, dest: LAUNCHER.into() }, Vec::new(), Mode::Applet).await?;
    let mut notes = started.notes;
    drop(started.output);
    // Until the NRO has stopped it, the old sysmodule may still answer.
    let start = tokio::time::Instant::now();
    loop {
        client.disconnect().await;
        if client.hello().await.is_ok_and(|info| info.kind == DaemonKind::Nro) {
            break;
        }
        if start.elapsed() > NRO_DAEMON_WAIT {
            bail!("the Kickstand NRO didn't take over from the sysmodule within {NRO_DAEMON_WAIT:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    notes.push(format!("the Kickstand NRO stopped the sysmodule after {:.1} s", start.elapsed().as_secs_f64()));
    client.handover().await?;
    let info = client.hello().await?;
    notes.push(format!("the sysmodule is back: {} (protocol {})", info.version, info.protocol));
    // The NRO exited to hbmenu, still in the Album.
    if let Some(app) = status(client, Mode::Applet).await? {
        close(client, Mode::Applet, &app).await?;
        notes.push("closed the Album".into());
    }
    Ok(notes)
}

/// The user's games, which need full memory: the projects in projects.json, recognised by where they
/// deploy to or by the NRO's file name (a build sent somewhere else).
pub(crate) fn is_game(path: &str, projects: &std::collections::BTreeMap<String, crate::project::Project>) -> bool {
    let file = |p: &str| p.rsplit('/').next().unwrap_or(p).to_ascii_lowercase();
    projects.values().any(|project| {
        let built = project.nro.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase());
        path.eq_ignore_ascii_case(&project.dest) || file(path) == file(&project.dest) || built.is_some_and(|b| b == file(path))
    })
}

/// Notes that the NRO in process `pid` exited (its stdout closed), so that process shows hbmenu again.
pub fn nro_exited(pid: u64) {
    if pid != 0 {
        HBMENU_PID.store(pid, Ordering::Relaxed);
    }
}

/// Presses Y in hbmenu until NetLoader accepts a connection (hbmenu may still be loading at first).
async fn start_netloader(client: &Client, wait: Duration) -> Result<TcpStream> {
    let start = tokio::time::Instant::now();
    let mut pressed_at = None;
    loop {
        if pressed_at.is_none_or(|t: tokio::time::Instant| t.elapsed() > Duration::from_secs(4)) {
            client.press(vec![Button::Y], 1, 100).await?;
            pressed_at = Some(tokio::time::Instant::now());
        }
        match nxlink::connect(client.host()).await {
            Ok(sock) => return Ok(sock),
            Err(e) if start.elapsed() > wait => {
                return Err(e).context("hbmenu's NetLoader didn't start listening after Y");
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Most NRO output kept for the agent; older output is dropped first.
const OUTPUT_KEEP: usize = 1 << 20;

/// An NRO's stdout, collected in the background, for the agent to read in pieces.
#[derive(Default)]
pub struct OutputLog {
    text: String,
    /// Bytes of `text` already returned by [`OutputLog::take_new`].
    read: usize,
    dropped: usize,
    open: bool,
    /// When the NRO last wrote anything.
    last_output: Option<std::time::Instant>,
}

impl OutputLog {
    /// Collects `stream` into `log` a line at a time, symbolizing backtrace addresses when `symbols` is
    /// given, until the NRO closes it (it exited) or a new run replaces it.
    pub fn collect(
        log: &std::sync::Arc<std::sync::Mutex<OutputLog>>,
        stream: TcpStream,
        symbols: Option<crate::symbolize::Shared>,
        pid: u64,
    ) -> tokio::task::JoinHandle<()> {
        *log.lock().unwrap() = OutputLog { open: true, ..Default::default() };
        let log = log.clone();
        tokio::spawn(async move {
            let mut lines = Lines::new(stream, symbols);
            while let Some(line) = lines.next().await {
                log.lock().unwrap().push(&line);
            }
            log.lock().unwrap().open = false;
            nro_exited(pid);
        })
    }

    fn push(&mut self, s: &str) {
        self.last_output = Some(std::time::Instant::now());
        self.text.push_str(s);
        if self.text.len() > OUTPUT_KEEP {
            let mut cut = self.text.len() - OUTPUT_KEEP;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.read = self.read.saturating_sub(cut);
            self.dropped += cut;
        }
    }

    /// Output not returned before, and whether the NRO still has its stdout open.
    pub fn take_new(&mut self) -> (String, bool) {
        let mut new = String::new();
        if self.dropped > 0 && self.read == 0 {
            new.push_str(&format!("[{} earlier bytes dropped]\n", self.dropped));
        }
        new.push_str(&self.text[self.read..]);
        self.read = self.text.len();
        (new, self.open)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn last_output(&self) -> Option<std::time::Instant> {
        self.last_output
    }

    /// The last `n` lines, without marking anything read.
    pub fn tail(&self, n: usize) -> String {
        let start = self.text.rmatch_indices('\n').nth(n).map_or(0, |(i, _)| i + 1);
        self.text[start..].to_string()
    }
}

/// An NRO's output as whole lines (so a backtrace address split across reads still matches), each
/// followed by its symbolized addresses.
pub struct Lines {
    stream: TcpStream,
    symbols: Option<crate::symbolize::Shared>,
    pending: Vec<u8>,
    done: bool,
}

impl Lines {
    pub fn new(stream: TcpStream, symbols: Option<crate::symbolize::Shared>) -> Self {
        Lines { stream, symbols, pending: Vec::new(), done: false }
    }

    /// The next line with its newline (the last may have none), or `None` once the NRO closes its stdout.
    pub async fn next(&mut self) -> Option<String> {
        use tokio::io::AsyncReadExt;
        loop {
            let line = if let Some(i) = self.pending.iter().position(|&b| b == b'\n') {
                self.pending.drain(..=i).collect::<Vec<u8>>()
            } else if self.done {
                if self.pending.is_empty() {
                    return None;
                }
                std::mem::take(&mut self.pending)
            } else {
                let mut buf = [0; 4096];
                match self.stream.read(&mut buf).await {
                    Ok(n @ 1..) => self.pending.extend_from_slice(&buf[..n]),
                    _ => self.done = true,
                }
                continue;
            };
            let line = String::from_utf8_lossy(&line).into_owned();
            return Some(match &self.symbols {
                Some(symbols) => symbols.lock().unwrap().annotate(&line),
                None => line,
            });
        }
    }
}
