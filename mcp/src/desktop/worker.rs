// SPDX-License-Identifier: GPL-3.0-only
//! The desktop app's network side: a thread with its own runtime that finds consoles, pairs, and keeps an
//! eye on the paired one, writing what it learns into a [`Model`] the UI draws from.
//!
//! Watching uses [`proto::Request::Activity`] and [`proto::Request::Agents`] only, which the console doesn't
//! count as activity: an open window neither shows up as an agent nor keeps the Switch awake.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use proto::{ActivityReport, AgentsReport, HekateTarget};
use tokio::sync::mpsc;

use crate::client::{Client, DaemonInfo};
use crate::link::{self, App};
use crate::pairing::{self, Found};
use crate::{config, tools, update};

/// How often the paired console is asked what's going on.
const POLL: Duration = Duration::from_secs(2);
/// How often to look for a newer release.
const UPDATE_CHECK: Duration = Duration::from_secs(6 * 3600);
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
    /// Make the console in use forget this PC, and stop using it.
    Unpair,
    /// Replace this program with the newer release found, then start it again.
    UpdatePc,
    /// Put this PC's version of Kickstand on the console in use.
    UpdateSwitch,
    /// The user's answer to an agent's question ([`crate::questions`]).
    Answer { id: String, answer: crate::questions::Answer },
    /// Start recording the controllers for a macro.
    RecordStart,
    /// Stop recording and save it.
    RecordStop { name: String, note: String },
    /// Stop recording and throw it away.
    RecordCancel,
    PlayMacro { name: String },
    /// Pause, resume or stop the macro playing, whoever plays it.
    Playback(proto::PlaybackAction),
    DeleteMacro { name: String },
}

/// [`Model::busy`] while this app plays a macro.
pub const PLAYING: &str = "Playing the macro";

/// The longest recording the desktop app starts.
pub const RECORD_MAX_S: u32 = 300;

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
    pub labels: Labels,
    /// The console in use has just been unpaired: the UI goes back to finding one.
    pub unpaired: bool,
    /// A release newer than this program, once one is found.
    pub update: Option<update::Release>,
    /// This program was just updated: the UI starts the new copy here and closes.
    pub restart: Option<std::path::PathBuf>,
    /// Agents' questions for the user, oldest first, from AI apps that can't ask themselves.
    pub questions: Vec<crate::questions::Question>,
    /// The saved macros, by name.
    pub macros: Vec<crate::macros::Listed>,
    /// When the recording running now began.
    pub recording: Option<std::time::Instant>,
}

/// What to call each agent ("Gemini Agent #2"): numbered per maker as first seen, kept while the app runs.
/// One agent is one MCP server process (its session tag), however often it reconnects; a terminal command
/// is "Terminal", unnumbered.
#[derive(Default)]
pub struct Labels {
    /// By connection id: what it introduced itself as, and its label.
    by_id: HashMap<u32, (Option<String>, String)>,
    /// By app name and session tag: the label given to that agent.
    by_session: HashMap<(String, String), String>,
    counts: HashMap<String, u32>,
}

impl Labels {
    pub fn get(&self, id: u32) -> Option<&str> {
        self.by_id.get(&id).map(|(_, label)| label.as_str())
    }

    /// The label of the agent that introduced itself as `intro`, if it has one yet.
    pub fn of(&self, intro: &str) -> Option<&str> {
        self.by_id.values().find(|(was, _)| was.as_deref() == Some(intro)).map(|(_, label)| label.as_str())
    }

    /// Labels the agents and events of `report` not seen yet (ids from older consoles, 0, aren't real).
    fn assign(&mut self, report: &AgentsReport, links: &[(App, link::State)]) {
        let mut seen: Vec<(u32, Option<&str>)> = report.agents.iter().filter(|a| !a.you).map(|a| (a.id, a.app.as_deref())).collect();
        seen.extend(report.recent.iter().map(|e| (e.id, e.app.as_deref())));
        seen.sort_by_key(|(id, _)| *id);
        for (id, intro) in seen {
            // The same id for another app: the console restarted and counts from 1 again.
            if id == 0 || self.by_id.get(&id).is_some_and(|(was, _)| was.as_deref() == intro) {
                continue;
            }
            let (name, session) = match intro.map(crate::client::split_introduction) {
                Some((name, session)) => (Some(name), session),
                None => (None, None),
            };
            let label = match (name, session) {
                (Some(crate::client::TERMINAL), _) => "Terminal".to_string(),
                (Some(crate::client::DESKTOP), _) => "Desktop app".to_string(),
                (Some(name), Some(session)) => match self.by_session.get(&(name.to_string(), session.to_string())) {
                    Some(label) => label.clone(),
                    None => {
                        let label = self.next(base(name, links));
                        self.by_session.insert((name.to_string(), session.to_string()), label.clone());
                        label
                    }
                },
                (Some(name), None) => self.next(base(name, links)),
                (None, _) => self.next("Agent".to_string()),
            };
            self.by_id.insert(id, (intro.map(String::from), label));
        }
    }

    /// "<base> #n", the next number for `base`.
    fn next(&mut self, base: String) -> String {
        let count = self.counts.entry(base.clone()).or_default();
        *count += 1;
        format!("{base} #{count}")
    }
}

/// "Claude Agent" for an agent in `name` (an MCP client's name, or a `link` slug).
pub fn base(name: &str, links: &[(App, link::State)]) -> String {
    match App::identify(name, |app| linked(links, app)) {
        Some(app) if app.maker() == link::Maker::Custom => format!("{} Agent", app.name()),
        Some(app) => format!("{} Agent", app.maker().name()),
        None => format!("{} Agent", capitalize(name)),
    }
}

pub fn linked(links: &[(App, link::State)], app: App) -> bool {
    links.iter().any(|(a, s)| *a == app && matches!(s, link::State::Linked { .. }))
}

#[derive(Default)]
pub enum Reach {
    #[default]
    Unknown,
    Connected(Live),
    Unreachable(String),
}

impl Reach {
    /// What the console reported, while it's answering.
    pub fn live(&self) -> Option<&Live> {
        match self {
            Reach::Connected(live) => Some(live),
            _ => None,
        }
    }
}

pub struct Live {
    pub info: DaemonInfo,
    /// `None` from a console older than protocol 22 (both).
    pub activity: Option<ActivityReport>,
    pub agents: Option<AgentsReport>,
    /// Whether a macro is playing (`None` from a console older than protocol 27).
    pub playback: Option<proto::Playback>,
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
        macros: crate::macros::list().unwrap_or_default(),
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
        let updates = tokio::spawn(self.clone().update_loop());
        let questions = tokio::spawn(self.clone().question_loop());
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
        updates.abort();
        questions.abort();
    }

    /// Says the window is open, and picks up agents' questions for the user, every second.
    async fn question_loop(self) {
        loop {
            let found = tokio::task::spawn_blocking(|| {
                crate::questions::heartbeat();
                crate::questions::pending()
            })
            .await
            .unwrap_or_default();
            if self.model.lock().unwrap().questions != found {
                self.update(|m| m.questions = found);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Looks for a newer release now and then (quietly: offline, it just finds nothing).
    async fn update_loop(self) {
        loop {
            if let Ok(Ok(Some(release))) = tokio::task::spawn_blocking(update::newer).await {
                self.update(|m| m.update = Some(release));
            }
            tokio::time::sleep(UPDATE_CHECK).await;
        }
    }

    /// Asks the paired console who's connected and what they're doing, and looks again at which AI apps
    /// are installed and linked (one may have been installed or set up since).
    async fn poll(&self) {
        let links = tokio::task::spawn_blocking(|| App::ALL.iter().map(|&app| (app, link::state(app))).collect::<Vec<_>>()).await;
        if let Ok(links) = links {
            self.update(|m| m.links = links);
        }
        let Some(client) = self.client.clone() else { return };
        let result = async {
            let info = client.hello().await?;
            let activity = client.activity().await?;
            let agents = client.agents().await?;
            let playback = if info.protocol >= 27 { Some(client.playback(proto::PlaybackAction::Query).await?) } else { None };
            anyhow::Ok(Live { info, activity, agents, playback, at: Instant::now() })
        }
        .await;
        self.update(|m| {
            if let Ok(Live { agents: Some(agents), .. }) = &result {
                m.labels.assign(agents, &m.links);
            }
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
            Cmd::Unpair => self.unpair().await,
            Cmd::UpdatePc => {
                let Some(release) = self.model.lock().unwrap().update.clone() else { return };
                self.update(|m| m.busy = Some("Updating Kickstand"));
                let worker = self.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || update::update_pc(&release)).await.map_err(anyhow::Error::from).and_then(|r| r);
                    worker.update(|m| {
                        m.busy = None;
                        match result {
                            Ok(exe) => m.restart = Some(exe),
                            Err(e) => m.notice = Some((format!("Couldn't update: {e:#}"), true)),
                        }
                    });
                });
            }
            Cmd::Answer { id, answer } => {
                let result = crate::questions::answer(&id, answer);
                self.update(|m| {
                    m.questions.retain(|q| q.id != id);
                    if let Err(e) = result {
                        m.notice = Some((format!("Couldn't pass on your answer: {e:#}"), true));
                    }
                });
            }
            Cmd::RecordStart => {
                let Some(client) = self.client.clone() else { return };
                match client.record_start(RECORD_MAX_S * 1000).await {
                    Ok(()) => self.update(|m| {
                        m.recording = Some(std::time::Instant::now());
                        m.notice = None;
                    }),
                    Err(e) => self.update(|m| m.notice = Some((format!("Couldn't start recording: {e:#}"), true))),
                }
            }
            Cmd::RecordStop { name, note } => {
                let Some(client) = self.client.clone() else { return };
                self.update(|m| {
                    m.recording = None;
                    m.busy = Some("Saving the macro");
                });
                let result = async {
                    let (inputs, ms, full) = client.record_stop().await?;
                    let data = crate::macros::from_recording(&inputs, ms, note)?;
                    crate::macros::save(&name, &data)?;
                    anyhow::Ok(format!("Saved {name} ({}){}", crate::macros::length(data.ms), if full { ": it filled up and stopped early" } else { "" }))
                }
                .await;
                self.update(|m| {
                    m.busy = None;
                    m.macros = crate::macros::list().unwrap_or_default();
                    m.notice = Some(match result {
                        Ok(text) => (text, false),
                        Err(e) => (format!("Couldn't save it: {e:#}"), true),
                    });
                });
            }
            Cmd::RecordCancel => {
                if let Some(client) = self.client.clone() {
                    let _ = client.record_stop().await;
                }
                self.update(|m| m.recording = None);
            }
            Cmd::PlayMacro { name } => self.button(PLAYING, move |client| async move {
                // On a connection of its own, so watching (and Pause and Stop) carry on meanwhile.
                let replayed = crate::macros::play(&client.another(), &name).await?;
                Ok(capitalize(&replayed.summary()))
            }),
            Cmd::Playback(action) => {
                let Some(client) = self.client.clone() else { return };
                let result = client.playback(action).await;
                let now = client.playback(proto::PlaybackAction::Query).await.ok();
                self.update(|m| {
                    if let Reach::Connected(live) = &mut m.reach
                        && now.is_some()
                    {
                        live.playback = now;
                    }
                    m.notice = Some(match result {
                        Ok(was) => (capitalize(&crate::macros::control_text(&was, action)), false),
                        Err(e) => (format!("{e:#}"), true),
                    });
                });
            }
            Cmd::DeleteMacro { name } => {
                let result = crate::macros::delete(&name);
                self.update(|m| {
                    m.macros = crate::macros::list().unwrap_or_default();
                    m.notice = Some(match result {
                        Ok(()) => (format!("Deleted {name}"), false),
                        Err(e) => (format!("{e:#}"), true),
                    });
                });
            }
            Cmd::UpdateSwitch => self.button("Updating the Switch", |client| async move {
                update::update_switch(&client).await?;
                Ok(format!("The Switch has Kickstand {}", update::VERSION))
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

    async fn unpair(&mut self) {
        let (Some(client), Some(host)) = (self.client.clone(), self.model.lock().unwrap().host.clone()) else { return };
        self.update(|m| m.busy = Some("Unpairing"));
        let result = async {
            client.unpair().await?;
            config::forget_host(&host)
        }
        .await;
        match result {
            Ok(_) => {
                self.client = None;
                self.update(|m| {
                    m.busy = None;
                    m.host = None;
                    m.name = None;
                    m.reach = Reach::Unknown;
                    m.unpaired = true;
                });
            }
            Err(e) => self.update(|m| {
                m.busy = None;
                m.notice = Some((format!("Couldn't unpair: {e:#}"), true));
            }),
        }
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
    Some(Arc::new(Client::new(format!("{host}:{}", proto::DEFAULT_PORT), token).named(crate::client::DESKTOP)))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: u32, app: &str) -> proto::AgentInfo {
        proto::AgentInfo {
            id,
            ip: [192, 168, 1, 9],
            you: false,
            app: Some(app.into()),
            connected_ms: 0,
            requests: 1,
            doing: "checking on the console".into(),
            idle_ms: 0,
            change: None,
        }
    }

    #[test]
    fn one_agent_keeps_its_label_across_reconnections() {
        let mut labels = Labels::default();
        let report = |agents| AgentsReport { agents, recent: Vec::new() };
        labels.assign(&report(vec![agent(1, "code\u{1f}aaaaaa"), agent(2, "kickstand-cli\u{1f}cccccc")]), &[]);
        // The first agent reconnected (new connection id, same session); a second session started.
        labels.assign(&report(vec![agent(5, "code\u{1f}aaaaaa"), agent(6, "code\u{1f}bbbbbb"), agent(7, "kickstand-cli\u{1f}dddddd")]), &[]);
        assert_eq!(labels.get(1), labels.get(5));
        assert!(labels.get(5).unwrap().ends_with("Agent #1"), "{:?}", labels.get(5));
        assert!(labels.get(6).unwrap().ends_with("Agent #2"), "{:?}", labels.get(6));
        assert_eq!((labels.get(2), labels.get(7)), (Some("Terminal"), Some("Terminal")));
    }
}
