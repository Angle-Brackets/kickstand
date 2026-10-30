// SPDX-License-Identifier: GPL-3.0-only
//! Opening hbmenu with nobody at the console: launch a title from the home menu while holding R, which
//! Atmosphère turns into hbloader with the title's full memory. Not every title works (ones that check
//! online rights, game cards that aren't inserted), and only the home menu's row can do it, not "All
//! Software". So the first time, try each tile of the row in order; remember the one that worked (the
//! host title, in `~/.config/kickstand/host-title`) and go straight to it afterwards.
//!
//! Running an NRO then goes through hbmenu's NetLoader ([`run_nro`]): an NRO from this PC is sent as is;
//! one already on the SD card is opened by sending the Kickstand NRO with `--open <path>`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use proto::{App, AppEntry, Button, HOME_MENU_TILES, InputStep};
use tokio::net::TcpStream;

use crate::client::{Client, check_path};
use crate::{config, nxlink};

/// Shortest wait for the home menu to come back after an application closes.
const AFTER_CLOSE: Duration = Duration::from_millis(1500);
/// How long to look for a new application process after the last input.
const PROCESS_WAIT: Duration = Duration::from_secs(5);

pub struct Opened {
    pub host: AppEntry,
    /// What was tried on the way, for the agent to relay.
    pub notes: Vec<String>,
}

fn step(buttons: &[Button], ms: u32) -> InputStep {
    InputStep { buttons: buttons.to_vec(), ms }
}

/// Opens hbmenu, from whatever is running. Fails with what it tried if no title on the row works.
pub async fn open_hbmenu(client: &Client) -> Result<Opened> {
    let mut notes = Vec::new();
    if let Some(app) = client.app_status().await? {
        if app.hbl == Some(true) {
            let host = AppEntry { id: app.program_id, name: String::new() };
            notes.push("hbloader was already running (hbmenu or an NRO)".into());
            return Ok(Opened { host, notes });
        }
        close(client, &app).await?;
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
        match try_tile(client, tile).await? {
            Some(app) if app.hbl == Some(true) => {
                let host = apps.iter().find(|a| a.id == app.program_id).cloned().unwrap_or(AppEntry { id: app.program_id, name: String::new() });
                config::save_host_title(host.id, &host.name)?;
                notes.push(format!("hbmenu opened through {label} (tile {tile})"));
                return Ok(Opened { host, notes });
            }
            Some(app) => {
                let how = if app.hbl.is_none() { "Atmosphère didn't say whether it was hbloader" } else { "it started as the game" };
                notes.push(format!("{label} (tile {tile}): {how}"));
                close(client, &app).await?;
            }
            None => {
                notes.push(format!("{label} (tile {tile}): nothing started (it needs a card, rights or an online check)"));
                dismiss_dialogs(client).await?;
            }
        }
    }
    bail!("no title on the home menu's row opened hbmenu:\n- {}", notes.join("\n- "))
}

/// Starts tile `tile` of the home menu's row with R held; returns the application that started, if any.
async fn try_tile(client: &Client, tile: usize) -> Result<Option<App>> {
    // HOME on the home menu selects the first tile.
    let mut steps = vec![step(&[Button::Home], 100), step(&[], 1200)];
    for _ in 0..tile {
        steps.extend([step(&[Button::Right], 80), step(&[], 150)]);
    }
    client.input(steps).await?;
    // Start it, holding R long enough to cover the process starting. A title that asks who is playing
    // only starts once that's answered, so nothing is running yet: confirm the first user, still on R.
    let launch = vec![step(&[Button::R], 300), step(&[Button::R, Button::A], 150), step(&[Button::R], 6000)];
    client.input(launch.clone()).await?;
    if let Some(app) = client.app_status().await? {
        return Ok(Some(app));
    }
    client.input(launch).await?;
    wait_for_app(client).await
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

/// Terminates `app` and waits for the home menu to take over again.
async fn close(client: &Client, app: &App) -> Result<()> {
    client.close_app().await?;
    let start = tokio::time::Instant::now();
    while client.app_status().await?.is_some_and(|now| now.pid == app.pid) {
        if start.elapsed() > PROCESS_WAIT {
            bail!("application {:016x} didn't close", app.program_id);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tokio::time::sleep(AFTER_CLOSE).await;
    Ok(())
}

/// The Kickstand NRO, whose launcher mode opens an NRO already on the SD card.
const LAUNCHER: &str = "/switch/kickstand/kickstand.nro";
/// How long NetLoader gets to start listening after Y.
const NETLOADER_WAIT: Duration = Duration::from_secs(15);
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
    /// The NRO's stdout, if it connected one (libnx's nxlink stdio, or the `nxlink` crate).
    pub output: Option<TcpStream>,
}

/// Starts an NRO with `args` from a fresh hbmenu: whatever runs (hbmenu, an NRO, a game) is closed first.
pub async fn run_nro(client: &Client, program: Program, args: Vec<String>) -> Result<Started> {
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
    // hbmenu can't be told apart from an NRO, so start from a fresh one.
    if let Some(app) = client.app_status().await?
        && app.hbl == Some(true)
    {
        close(client, &app).await?;
        notes.push("closed the running hbloader (hbmenu or an NRO)".into());
    }
    notes.extend(open_hbmenu(client).await?.notes);

    let output = nxlink::OutputListener::bind().await?;
    let sock = start_netloader(client).await?;
    nxlink::send(sock, &nro, &dest, &args).await?;
    notes.push(format!("sent {} bytes as sdmc:/switch/{dest}", nro.len()));
    let output = output.accept(OUTPUT_WAIT).await?;
    if output.is_none() {
        notes.push("the NRO didn't connect its stdout (it may not use nxlink stdio)".into());
    }
    Ok(Started { notes, output })
}

/// Presses Y in hbmenu until NetLoader accepts a connection (hbmenu may still be loading at first).
async fn start_netloader(client: &Client) -> Result<TcpStream> {
    let start = tokio::time::Instant::now();
    let mut pressed_at = None;
    loop {
        if pressed_at.is_none_or(|t: tokio::time::Instant| t.elapsed() > Duration::from_secs(4)) {
            client.press(vec![Button::Y], 1, 100).await?;
            pressed_at = Some(tokio::time::Instant::now());
        }
        match nxlink::connect(client.host()).await {
            Ok(sock) => return Ok(sock),
            Err(e) if start.elapsed() > NETLOADER_WAIT => {
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
}

impl OutputLog {
    /// Collects `stream` into `log` a line at a time, symbolizing backtrace addresses when `symbols` is
    /// given, until the NRO closes it (it exited) or a new run replaces it.
    pub fn collect(
        log: &std::sync::Arc<std::sync::Mutex<OutputLog>>,
        stream: TcpStream,
        symbols: Option<crate::symbolize::Shared>,
    ) -> tokio::task::JoinHandle<()> {
        *log.lock().unwrap() = OutputLog { open: true, ..Default::default() };
        let log = log.clone();
        tokio::spawn(async move {
            let mut lines = Lines::new(stream, symbols);
            while let Some(line) = lines.next().await {
                log.lock().unwrap().push(&line);
            }
            log.lock().unwrap().open = false;
        })
    }

    fn push(&mut self, s: &str) {
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
