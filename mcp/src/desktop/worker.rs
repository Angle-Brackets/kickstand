// SPDX-License-Identifier: GPL-3.0-only
//! The desktop app's network side: a thread with its own runtime that finds consoles, pairs, and keeps an
//! eye on the paired one, writing what it learns into a [`Model`] the UI draws from.
//!
//! Watching uses [`proto::Request::Activity`] only, which the console doesn't count as activity: an open
//! window neither shows up as an agent nor keeps the Switch awake.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use proto::{ActivityReport, HekateTarget};
use tokio::sync::mpsc;

use crate::client::{Client, DaemonInfo};
use crate::link::{self, App};
use crate::pairing::{self, Found};
use crate::{config, tools};

/// How often the paired console is asked what's going on.
const POLL: Duration = Duration::from_secs(2);
/// How long a discovery round listens.
const DISCOVER_WAIT: Duration = Duration::from_millis(1200);

pub enum Cmd {
    Pair { addr: SocketAddr, code: String },
    /// Use this (already paired) console from now on.
    Open { host: String },
    /// Ask the console again now (after "Try again").
    Retry,
    Link(App),
    Unlink(App),
    Screenshot,
    KeepAwake { minutes: u32 },
    Reboot,
}

#[derive(Default)]
pub struct Model {
    /// The UI sets this while it shows the list of consoles.
    pub discovering: bool,
    pub found: Vec<Found>,
    /// A discovery round has finished since `discovering` was set.
    pub searched: bool,
    pub find_error: Option<String>,
    /// The paired console in use (`~/.config/kickstand/host`).
    pub host: Option<String>,
    pub reach: Reach,
    /// Its nickname, from discovery.
    pub name: Option<String>,
    pub links: Vec<(App, link::State)>,
    pub pairing: Pairing,
    /// A request from a button is running.
    pub busy: Option<&'static str>,
    /// The last button's outcome: (text, is an error).
    pub notice: Option<(String, bool)>,
}

#[derive(Default)]
pub enum Reach {
    #[default]
    Unknown,
    Connected(Live),
    Unreachable(String),
}

pub struct Live {
    pub info: DaemonInfo,
    /// `None` from a console older than protocol 22.
    pub activity: Option<ActivityReport>,
    pub at: Instant,
}

#[derive(Default, PartialEq, Eq)]
pub enum Pairing {
    #[default]
    Idle,
    Running,
    Done,
    Failed(String),
}

type Shared = Arc<Mutex<Model>>;

/// Starts the worker; returns the model and where to send commands.
pub fn spawn(ctx: egui::Context) -> (Shared, mpsc::UnboundedSender<Cmd>) {
    let model: Shared = Arc::new(Mutex::new(Model {
        host: config::host(None).ok(),
        links: App::ALL.iter().map(|&app| (app, link::state(app))).collect(),
        ..Default::default()
    }));
    let (tx, rx) = mpsc::unbounded_channel();
    let worker = Worker { model: model.clone(), ctx, client: None };
    std::thread::Builder::new()
        .name("kickstand-worker".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
            rt.block_on(worker.run(rx));
        })
        .expect("worker thread");
    (model, tx)
}

#[derive(Clone)]
struct Worker {
    model: Shared,
    ctx: egui::Context,
    client: Option<Arc<Client>>,
}

impl Worker {
    fn update(&self, f: impl FnOnce(&mut Model)) {
        f(&mut self.model.lock().unwrap());
        self.ctx.request_repaint();
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        self.client = self.model.lock().unwrap().host.clone().and_then(|h| connect_to(&h));
        if let Some(host) = self.model.lock().unwrap().host.clone() {
            tokio::spawn(self.clone().nickname(host));
        }
        let discovery = tokio::spawn(self.clone().discover_loop());
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.command(cmd).await,
                    None => break,
                },
                _ = tick.tick() => self.poll().await,
            }
        }
        discovery.abort();
    }

    /// Asks the paired console who's connected and what they're doing.
    async fn poll(&self) {
        let Some(client) = self.client.clone() else { return };
        let result = async {
            let info = client.hello().await?;
            let activity = client.activity().await?;
            anyhow::Ok(Live { info, activity, at: Instant::now() })
        }
        .await;
        self.update(|m| {
            m.reach = match result {
                Ok(live) => Reach::Connected(live),
                Err(e) => Reach::Unreachable(format!("{e:#}")),
            }
        });
    }

    async fn discover_loop(self) {
        loop {
            if !self.model.lock().unwrap().discovering {
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
            let result = pairing::discover(DISCOVER_WAIT, None).await;
            self.update(|m| {
                m.searched = true;
                match result {
                    Ok(found) => {
                        m.found = found;
                        m.find_error = None;
                    }
                    Err(e) => m.find_error = Some(format!("{e:#}")),
                }
            });
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }

    /// Learns the console's nickname with a probe straight to it.
    async fn nickname(self, host: String) {
        let Ok(ip) = host.parse() else { return };
        let to = SocketAddr::new(ip, proto::discovery::PORT);
        if let Ok(found) = pairing::discover(Duration::from_millis(900), Some(to)).await
            && let Some(f) = found.first()
            && !f.announce.nickname.is_empty()
        {
            let name = f.announce.nickname.clone();
            self.update(|m| m.name = Some(name));
        }
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Pair { addr, code } => {
                self.update(|m| m.pairing = Pairing::Running);
                let host = addr.ip().to_string();
                match pairing::pair(&host, addr.port(), &code).await {
                    Ok(_) => {
                        self.use_host(host);
                        self.update(|m| m.pairing = Pairing::Done);
                        self.poll().await;
                    }
                    Err(e) => self.update(|m| m.pairing = Pairing::Failed(format!("{e:#}"))),
                }
            }
            Cmd::Open { host } => {
                if let Err(e) = config::save_host(&host) {
                    self.update(|m| m.notice = Some((format!("couldn't save the console: {e:#}"), true)));
                }
                self.use_host(host);
                self.poll().await;
            }
            Cmd::Retry => self.poll().await,
            Cmd::Link(app) | Cmd::Unlink(app) => {
                let linking = matches!(cmd, Cmd::Link(_));
                let result = tokio::task::spawn_blocking(move || if linking { link::link(app) } else { link::unlink(app) }).await;
                let result = result.map_err(anyhow::Error::from).and_then(|r| r);
                self.update(|m| {
                    m.links = App::ALL.iter().map(|&app| (app, link::state(app))).collect();
                    m.notice = Some(match result {
                        Ok(text) => (capitalize(&text), false),
                        Err(e) => (format!("{}: {e:#}", app.name()), true),
                    });
                });
            }
            Cmd::Screenshot => self.button("Taking a screenshot", |client| async move {
                let jpeg = client.screenshot().await?;
                let path = tools::default_capture_path("jpg")?;
                std::fs::write(&path, jpeg)?;
                open_file(&path);
                Ok(format!("Saved {}", path.display()))
            }),
            Cmd::KeepAwake { minutes } => self.button("Keeping it awake", move |client| async move {
                let left = client.keep_awake(Some(minutes)).await?;
                Ok(format!("The Switch stays awake for {} min", left.div_ceil(60)))
            }),
            Cmd::Reboot => self.button("Restarting", |client| async move {
                client.reboot_to_hekate(HekateTarget::Menu).await?;
                Ok("The Switch is restarting into hekate's menu".into())
            }),
        }
    }

    /// Runs a button's request in the background, showing `what` meanwhile and the outcome after.
    fn button<F, Fut>(&self, what: &'static str, f: F)
    where
        F: FnOnce(Arc<Client>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<String>> + Send,
    {
        let Some(client) = self.client.clone() else { return };
        self.update(|m| m.busy = Some(what));
        let worker = self.clone();
        tokio::spawn(async move {
            let result = f(client).await;
            worker.update(|m| {
                m.busy = None;
                m.notice = Some(match result {
                    Ok(text) => (text, false),
                    Err(e) => (format!("{e:#}"), true),
                });
            });
        });
    }

    fn use_host(&mut self, host: String) {
        self.client = connect_to(&host);
        let name = self.model.lock().unwrap().found.iter().find(|f| f.addr.ip().to_string() == host).map(|f| f.announce.nickname.clone());
        self.update(|m| {
            m.host = Some(host.clone());
            m.reach = Reach::Unknown;
            m.name = name.filter(|n| !n.is_empty());
        });
        tokio::spawn(self.clone().nickname(host));
    }
}

fn connect_to(host: &str) -> Option<Arc<Client>> {
    let token = config::load_token().ok()?;
    Some(Arc::new(Client::new(format!("{host}:{}", proto::DEFAULT_PORT), token)))
}

pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// Opens `path` in the system's viewer.
fn open_file(path: &std::path::Path) {
    let mut command = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = command.arg(path).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}
