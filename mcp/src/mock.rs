// SPDX-License-Identifier: GPL-3.0-only
//! A stand-in daemon that serves a local directory as the "SD card". It follows the same protocol and
//! path rules as the real one, so the client and MCP tools can be tested without the console.
//! It is not a model of the console's quirks (FS result codes, memory limits, timing).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use proto::path;
use proto::{DaemonKind, DirEntry, Error, ErrorKind, PROTOCOL_VERSION, Request, Response, crc32};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The mock console: its "SD card", the tokens it accepts, and the pairing code its screen shows, if any.
pub struct Console {
    pub root: PathBuf,
    pub tokens: Mutex<Vec<String>>,
    pub pair_code: Option<String>,
    activity: Mutex<Activity>,
    /// A running application, for tests of closing one (none unless a test sets it).
    pub app: Mutex<MockApp>,
    playback: Mutex<MockPlayback>,
    /// When its audio "capture" started: a 440 Hz tone, one stream position per byte since then.
    audio_start: Instant,
    /// What the capture reports instead of capturing, for tests of the explanations.
    pub audio_state: Mutex<Option<proto::CaptureState>>,
}

/// Like the daemon's `playback` module: Plays take their steps' time, and can be paused and stopped.
#[derive(Default)]
struct MockPlayback {
    generation: u32,
    paused: bool,
    /// The connection whose Plays run.
    player: Option<u64>,
}

#[derive(Default)]
pub struct MockApp {
    pub running: Option<proto::App>,
    /// Like a hung program: Close software from the home menu doesn't end it; only terminating does.
    pub ignores_exit: bool,
    /// Like an NRO with Kickstand's hot-reload hook: on +, it claims `/kickstand/next-load` (reads, then
    /// deletes it) and records what it would pass to `envSetNextLoad` in [`MockApp::chainloaded`].
    pub next_load_hook: bool,
    /// The (path, argv) the hook claimed.
    pub chainloaded: Option<(String, String)>,
    /// Like an NRO linked with libkickstand: it notices `/kickstand/next-load` by itself (here: the next
    /// time the folder is listed) and claims it with no button press.
    pub libkickstand: bool,
    /// How many Presses arrived.
    pub presses: u32,
}

impl MockApp {
    /// What the hook does on the way out: claim the request (read, delete) and keep what it would load.
    fn claim_next_load(&mut self, root: &std::path::Path) {
        let file = root.join(&path::NEXT_LOAD[1..]);
        if let Ok(text) = std::fs::read_to_string(&file)
            && std::fs::remove_file(&file).is_ok()
            && let [load, argv, ..] = text.lines().collect::<Vec<_>>()[..]
        {
            self.chainloaded = Some((load.to_string(), argv.to_string()));
        }
    }
}

/// Who is connected and what they did, for [`Request::Activity`].
#[derive(Default)]
struct Activity {
    next_id: u64,
    clients: Vec<(u64, MockClient)>,
    recent: VecDeque<Event>,
}

struct Event {
    at: Instant,
    id: u64,
    ip: [u8; 4],
    app: Option<String>,
    what: &'static str,
}

struct MockClient {
    app: Option<String>,
    ip: [u8; 4],
    connected: Instant,
    requests: u32,
    doing: &'static str,
    last: Instant,
    change: Option<(&'static str, Instant)>,
}

impl Console {
    pub fn new(root: PathBuf, tokens: Vec<String>, pair_code: Option<String>) -> Self {
        Console {
            root,
            tokens: Mutex::new(tokens),
            pair_code,
            activity: Mutex::default(),
            app: Mutex::default(),
            playback: Mutex::default(),
            audio_start: Instant::now(),
            audio_state: Mutex::default(),
        }
    }

    fn connected(&self, from: SocketAddr) -> u64 {
        let ip = match from.ip() {
            std::net::IpAddr::V4(v4) => v4.octets(),
            std::net::IpAddr::V6(_) => [0; 4],
        };
        let mut activity = self.activity.lock().unwrap();
        activity.next_id += 1;
        let id = activity.next_id;
        let now = Instant::now();
        activity.clients.push((id, MockClient { app: None, ip, connected: now, requests: 0, doing: "connecting", last: now, change: None }));
        id
    }

    fn disconnected(&self, id: u64) {
        self.end_playing(id);
        self.activity.lock().unwrap().clients.retain(|(c, _)| *c != id);
    }

    /// Like the daemon's: every request but Activity counts.
    fn request(&self, id: u64, req: &Request) {
        let mut activity = self.activity.lock().unwrap();
        let doing = req.describe();
        let Some((_, client)) = activity.clients.iter_mut().find(|(c, _)| *c == id) else { return };
        let changed = client.doing != doing;
        client.requests += 1;
        client.doing = doing;
        client.last = Instant::now();
        if proto::changes_console(doing) {
            client.change = Some((doing, client.last));
        }
        let (ip, app) = (client.ip, client.app.clone());
        if changed && !proto::ROUTINE.contains(&doing) {
            if activity.recent.len() == proto::MAX_RECENT {
                activity.recent.pop_front();
            }
            activity.recent.push_back(Event { at: Instant::now(), id, ip, app, what: doing });
        }
    }

    /// The capture's stream position: bytes of audio since it started.
    fn audio_head(&self) -> u64 {
        proto::audio::bytes_for_ms(self.audio_start.elapsed().as_millis().min(u128::from(u32::MAX)) as u32)
    }

    fn audio_capture(&self) -> proto::AudioCapture {
        let state = self.audio_state.lock().unwrap().clone().unwrap_or(proto::CaptureState::Capturing);
        let head = self.audio_head();
        let capturing = state == proto::CaptureState::Capturing;
        proto::AudioCapture {
            state,
            program_id: Some(0x0100_0000_0000_1234),
            hbl: Some(false),
            video_capture: (!capturing).then_some(2),
            buffered_ms: if capturing { proto::audio::ms_for_bytes(head).min(u64::from(proto::audio::RING_MS)) as u32 } else { 0 },
            newest_age_ms: capturing.then_some(0),
            cursor: head,
        }
    }

    fn introduce(&self, id: u64, app: String) {
        if let Some((_, client)) = self.activity.lock().unwrap().clients.iter_mut().find(|(c, _)| *c == id) {
            client.app = Some(app);
        }
    }

    fn agents(&self, you: u64) -> proto::AgentsReport {
        let activity = self.activity.lock().unwrap();
        let ms = |t: Instant| t.elapsed().as_millis() as u64;
        let agents = activity
            .clients
            .iter()
            .map(|(id, c)| proto::AgentInfo {
                id: *id as u32,
                ip: c.ip,
                you: *id == you,
                app: c.app.clone(),
                connected_ms: ms(c.connected),
                requests: c.requests,
                doing: c.doing.into(),
                idle_ms: ms(c.last),
                change: c.change.map(|(what, at)| (what.into(), ms(at))),
            })
            .collect();
        let recent = activity
            .recent
            .iter()
            .rev()
            .map(|e| proto::AgentEvent { ago_ms: ms(e.at), id: e.id as u32, ip: e.ip, app: e.app.clone(), what: e.what.into() })
            .collect();
        proto::AgentsReport { agents, recent }
    }

    fn report(&self, you: u64) -> proto::ActivityReport {
        let activity = self.activity.lock().unwrap();
        let ms = |t: Instant| t.elapsed().as_millis() as u64;
        proto::ActivityReport {
            clients: activity
                .clients
                .iter()
                .map(|(id, c)| proto::ClientInfo {
                    ip: c.ip,
                    you: *id == you,
                    connected_ms: ms(c.connected),
                    requests: c.requests,
                    doing: c.doing.into(),
                    idle_ms: ms(c.last),
                })
                .collect(),
            recent: activity.recent.iter().rev().map(|e| proto::ActivityEvent { ago_ms: ms(e.at), ip: e.ip, what: e.what.into() }).collect(),
            keep_awake_s: 0,
        }
    }
}

#[cfg(test)]
pub async fn serve(listener: TcpListener, root: PathBuf, token: String) -> Result<()> {
    serve_console(listener, Arc::new(Console::new(root, vec![token], None))).await
}

pub async fn serve_console(listener: TcpListener, console: Arc<Console>) -> Result<()> {
    loop {
        let (stream, from) = listener.accept().await?;
        let console = console.clone();
        tokio::spawn(async move {
            let id = console.connected(from);
            let _ = handle(stream, &console, id).await;
            console.disconnected(id);
        });
    }
}

/// Answers discovery probes on `socket` like the daemon, pointing at TCP `port`, as a console called
/// `nickname`.
pub async fn answer_probes(socket: tokio::net::UdpSocket, port: u16, pairing: bool, nickname: String) -> Result<()> {
    use proto::discovery;
    let mut buf = [0u8; discovery::MAX_DATAGRAM];
    loop {
        let (len, from) = socket.recv_from(&mut buf).await?;
        let Some(probe) = discovery::decode_probe(&buf[..len]) else { continue };
        let announce = discovery::Announce {
            nonce: probe.nonce,
            protocol: PROTOCOL_VERSION,
            daemon_version: "mock".into(),
            kind: DaemonKind::Nro,
            port,
            nickname: nickname.clone(),
            pairing,
        };
        socket.send_to(&discovery::encode_announce(&announce), from).await?;
    }
}

/// The mock's application: homebrew with an NRO (header, then a data page holding a pointer to a heap
/// float) and a heap page.
pub const MOCK_APP: proto::App = proto::App { pid: 0x81, program_id: 0x0100_0000_0000_1000, hbl: Some(true) };
pub const MOCK_NRO: u64 = 0x800_0000;
pub const MOCK_HEAP: u64 = 0x1_0000_0000;

fn mock_memory() -> Vec<(u64, u32, Vec<u8>)> {
    let mut text = vec![0u8; 0x1000];
    text[0x10..0x14].copy_from_slice(b"NRO0");
    text[0x18..0x1C].copy_from_slice(&0x2000u32.to_le_bytes());
    let mut data = vec![0u8; 0x1000];
    data[8..16].copy_from_slice(&(MOCK_HEAP + 0x10).to_le_bytes());
    let mut heap = vec![0u8; 0x1000];
    heap[0x14..0x18].copy_from_slice(&1.5f32.to_le_bytes());
    vec![(MOCK_NRO, 5, text), (MOCK_NRO + 0x1000, 3, data), (MOCK_HEAP, 3, heap)]
}

fn mock_span(memory: &mut [(u64, u32, Vec<u8>)], address: u64, len: usize) -> Option<&mut [u8]> {
    let (base, _, data) = memory.iter_mut().find(|(base, _, data)| (*base..*base + data.len() as u64).contains(&address))?;
    let start = (address - *base) as usize;
    data.get_mut(start..start + len)
}

struct Upload {
    path: String,
    data: Vec<u8>,
    size: u64,
}

impl Console {
    fn end_playing(&self, id: u64) {
        let mut playback = self.playback.lock().unwrap();
        if playback.player == Some(id) {
            playback.player = None;
            playback.paused = false;
        }
    }

    fn control(&self, action: proto::PlaybackAction) -> proto::Playback {
        let mut playback = self.playback.lock().unwrap();
        let app = playback.player.and_then(|p| self.activity.lock().unwrap().clients.iter().find(|(c, _)| *c == p).and_then(|(_, c)| c.app.clone()));
        let was = proto::Playback { playing: playback.player.is_some(), paused: playback.paused, app };
        if was.playing {
            match action {
                proto::PlaybackAction::Query => {}
                proto::PlaybackAction::Pause => playback.paused = true,
                proto::PlaybackAction::Resume => playback.paused = false,
                proto::PlaybackAction::Stop => {
                    playback.generation += 1;
                    playback.player = None;
                    playback.paused = false;
                }
            }
        }
        was
    }

    /// Takes the steps' time (paused time not counted); false if stopped.
    async fn play(&self, steps: &[proto::InputStep], generation: u32) -> bool {
        const CHECK: std::time::Duration = std::time::Duration::from_millis(10);
        let mut left: std::time::Duration = steps.iter().map(|s| std::time::Duration::from_millis(u64::from(s.ms))).sum();
        loop {
            let (stopped, paused) = {
                let playback = self.playback.lock().unwrap();
                (playback.generation != generation, playback.paused)
            };
            if stopped {
                return false;
            }
            if left.is_zero() {
                return true;
            }
            tokio::time::sleep(CHECK.min(left)).await;
            if !paused {
                left = left.saturating_sub(CHECK);
            }
        }
    }
}

async fn handle(mut stream: TcpStream, console: &Console, id: u64) -> Result<()> {
    let root = console.root.as_path();
    let mut authed = false;
    // The token this connection presented.
    let mut session_token = None;
    let mut upload: Option<Upload> = None;
    let mut memory = mock_memory();
    let mut audio_replay: Option<(u64, u64)> = None;
    let mut audio_volume: u8 = 80;
    let mut audio_muted: bool = false;
    // While this connection's Plays run back to back: the generation they started at.
    let mut playing: Option<u32> = None;
    loop {
        let mut prefix = [0u8; 4];
        stream.read_exact(&mut prefix).await?;
        let Some(len) = proto::frame_len(prefix) else { return Ok(()) };
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await?;
        let Ok(req) = proto::decode::<Request>(&body) else {
            // Like the daemon: a request from a newer protocol gets an error, and the connection goes on.
            stream.write_all(&proto::encode(&Response::error(ErrorKind::Unsupported, "unknown request"))?).await?;
            continue;
        };
        let passive = matches!(
            req,
            Request::Activity | Request::Agents | Request::Introduce { .. } | Request::Playback { action: proto::PlaybackAction::Query }
        );
        if authed && !passive {
            console.request(id, &req);
        }
        if !matches!(req, Request::Play { .. } | Request::Playback { .. } | Request::Activity | Request::Agents | Request::Introduce { .. })
            && playing.take().is_some()
        {
            console.end_playing(id);
        }
        let resp = match req {
            Request::Hello { version, token: t } => {
                if !proto::compatible(version) {
                    Response::error(ErrorKind::VersionMismatch, "version")
                } else if !console.tokens.lock().unwrap().iter().any(|known| proto::tokens::same(known, &t)) {
                    Response::error(ErrorKind::Unauthorized, "wrong token")
                } else {
                    authed = true;
                    session_token = Some(t);
                    Response::Hello { version: PROTOCOL_VERSION, daemon_version: "mock".into(), kind: DaemonKind::Nro }
                }
            }
            Request::Pair { version, code, token } => match &console.pair_code {
                _ if !proto::compatible(version) => Response::error(ErrorKind::VersionMismatch, "version"),
                None => Response::error(ErrorKind::Unsupported, "not pairing"),
                Some(shown) if !proto::tokens::same(shown, &code) => Response::error(ErrorKind::Unauthorized, "wrong code"),
                Some(_) if !proto::valid_token(&token) => Response::error(ErrorKind::BadRequest, "bad token"),
                Some(_) => {
                    let mut tokens = console.tokens.lock().unwrap();
                    if !tokens.contains(&token) {
                        tokens.push(token.clone());
                    }
                    authed = true;
                    session_token = Some(token);
                    Response::Hello { version: PROTOCOL_VERSION, daemon_version: "mock".into(), kind: DaemonKind::Nro }
                }
            },
            _ if !authed => Response::error(ErrorKind::Unauthorized, "send Hello first"),
            Request::Activity => Response::Activity(console.report(id)),
            Request::Agents => Response::Agents(console.agents(id)),
            Request::Introduce { app } => {
                console.introduce(id, app);
                Response::Ok
            }
            Request::Unpair => {
                let token = session_token.take().unwrap_or_default();
                console.tokens.lock().unwrap().retain(|t| t != &token);
                authed = false;
                Response::Ok
            }
            Request::Ping { nonce } => Response::Pong { nonce, uptime_ms: 1 },
            Request::UsbOpen => Response::error(ErrorKind::Unsupported, "the mock console has no USB port"),
            Request::UsbClose => Response::Ok,
            Request::ListDir { path } => match path::check_read(&path) {
                Ok(p) => {
                    let mut app = console.app.lock().unwrap();
                    if app.libkickstand && app.running.is_some() && path::NEXT_LOAD.starts_with(&format!("{p}/")) {
                        app.claim_next_load(root);
                    }
                    drop(app);
                    list(&root.join(&p[1..]))
                }
                Err(e) => Response::error(ErrorKind::Forbidden, e.describe()),
            },
            Request::Stat { path } => match path::check_read(&path) {
                Ok(p) => stat(&root.join(&p[1..])),
                Err(e) => Response::error(ErrorKind::Forbidden, e.describe()),
            },
            Request::ReadFile { path, offset, len } => match path::check_read(&path) {
                Ok(p) => read(&root.join(&p[1..]), offset, len),
                Err(e) => Response::error(ErrorKind::Forbidden, e.describe()),
            },
            Request::UploadBegin { path, size } => match path::check_write(&path) {
                Ok(p) => {
                    upload = Some(Upload { path: p, data: Vec::new(), size });
                    Response::UploadReady { staging_path: path::STAGING_DIR.into() }
                }
                Err(e) => Response::error(ErrorKind::Forbidden, e.describe()),
            },
            Request::UploadChunk { offset, data } => match upload.as_mut() {
                Some(up) if offset == up.data.len() as u64 && offset + data.len() as u64 <= up.size => {
                    up.data.extend_from_slice(&data);
                    Response::ChunkOk { received: up.data.len() as u64 }
                }
                _ => Response::error(ErrorKind::UploadState, "bad chunk"),
            },
            Request::UploadFinish { crc32: expected } => match upload.take() {
                Some(up) if up.data.len() as u64 == up.size => {
                    if crc32::checksum(&up.data) != expected {
                        Response::error(ErrorKind::ChecksumMismatch, "crc")
                    } else {
                        let dest = root.join(&up.path[1..]);
                        let written = std::fs::create_dir_all(dest.parent().unwrap())
                            .and_then(|()| std::fs::write(&dest, &up.data));
                        match written {
                            Ok(()) => Response::UploadDone { path: up.path, size: up.size },
                            Err(e) => io_err(e),
                        }
                    }
                }
                _ => Response::error(ErrorKind::UploadState, "incomplete"),
            },
            Request::UploadAbort => {
                upload = None;
                Response::Ok
            }
            Request::SysmoduleStatus | Request::SysmoduleStart | Request::SysmoduleStop => {
                // No processes to manage: report what's on the "SD card", never running.
                let installed = root.join(&path::SYSMODULE_NSP[1..]).exists();
                let boot2 = root.join(&path::SYSMODULE_BOOT2_FLAG[1..]).exists();
                Response::Sysmodule(proto::SysmoduleStatus { installed, boot2, pid: None })
            }
            Request::IramPeek { .. } => Response::error(ErrorKind::Unsupported, "the mock has no IRAM"),
            Request::Input { steps } => {
                // X then A on the home menu: Close software, confirmed.
                let pressed = |b: proto::Button| steps.iter().position(|s| s.buttons.contains(&b));
                let mut app = console.app.lock().unwrap();
                if let (Some(x), Some(a)) = (pressed(proto::Button::X), pressed(proto::Button::A))
                    && x < a
                    && !app.ignores_exit
                {
                    app.running = None;
                }
                Response::Ok
            }
            Request::CloseApp => Response::App(console.app.lock().unwrap().running.take()),
            Request::AppStatus => Response::App(console.app.lock().unwrap().running.clone()),
            Request::Play { steps, .. } => {
                let generation = *playing.get_or_insert_with(|| {
                    let mut playback = console.playback.lock().unwrap();
                    playback.player = Some(id);
                    playback.paused = false;
                    playback.generation
                });
                if console.play(&steps, generation).await { Response::Ok } else { Response::Playback(proto::Playback::default()) }
            }
            Request::Playback { action } => Response::Playback(console.control(action)),
            Request::Press { buttons, .. } => {
                let mut app = console.app.lock().unwrap();
                if buttons.contains(&proto::Button::Plus) && app.running.is_some() && app.next_load_hook {
                    app.claim_next_load(root);
                }
                app.presses += 1;
                Response::Ok
            }
            Request::Hold { .. }
            | Request::TypeText { .. }
            | Request::RecordStart { .. }
            | Request::HblOverride { .. }
            | Request::MemoryTarget { .. }
            | Request::IndicatorPosition { .. } => Response::Ok,
            Request::FatalReports => Response::Dir { entries: Vec::new(), truncated: false },
            // Every recording is the same: A, then the left stick up for a second.
            Request::RecordStop => Response::Recording { total: MOCK_RECORDING.len() as u32, ms: 2000, full: false },
            Request::RecordRead { from } => Response::Recorded { inputs: MOCK_RECORDING.iter().skip(from as usize).copied().collect() },
            Request::Find { dir, name, max } => match path::check_read(&dir) {
                Ok(p) => {
                    let mut entries = Vec::new();
                    find(root, &p, &name.to_lowercase(), max.max(1) as usize, &mut entries);
                    let truncated = entries.len() > max.max(1) as usize;
                    entries.truncate(max.max(1) as usize);
                    Response::Found { entries, truncated }
                }
                Err(e) => Response::error(ErrorKind::Forbidden, e.describe()),
            },
            Request::Grep { path, needle, ignore_case, max } => match path::check_read(&path) {
                Ok(p) => grep(root, &p, &needle, ignore_case, max.max(1) as usize),
                Err(e) => Response::error(ErrorKind::Forbidden, e.describe()),
            },
            Request::AppletStatus | Request::CloseApplet => Response::App(None),
            Request::ListApps { .. } => Response::Apps(Vec::new()),
            Request::Status => Response::Status(proto::ConsoleStatus {
                uptime_ms: 1,
                unlocking_ms: None,
                keep_awake_s: 0,
                application: console.app.lock().unwrap().running.clone(),
                applet: None,
            }),
            Request::Threads => Response::Threads {
                app: MOCK_APP,
                threads: vec![proto::ThreadInfo {
                    id: 0x5a,
                    pc: MOCK_NRO + 0x100,
                    lr: MOCK_NRO + 0x204,
                    sp: MOCK_HEAP + 0x800,
                    fp: MOCK_HEAP + 0x810,
                    frames: vec![MOCK_NRO + 0x204, 0x1234],
                }],
            },
            Request::MemoryMap => Response::Memory(proto::MemoryMap {
                app: MOCK_APP,
                heap: Some((MOCK_HEAP, 0x1000)),
                regions: memory
                    .iter()
                    .map(|(base, perm, data)| proto::MemoryRegion {
                        base: *base,
                        size: data.len() as u64,
                        state: if *base == MOCK_HEAP { 5 } else { 3 },
                        perm: *perm,
                        nro: data.get(0x10..0x14) == Some(b"NRO0"),
                    })
                    .collect(),
                truncated: false,
            }),
            Request::ReadMemory { address, len } => match mock_span(&mut memory, address, len as usize) {
                Some(span) => Response::Data { data: span.to_vec(), file_size: u64::from(len) },
                None => Response::error(ErrorKind::System, "unmapped"),
            },
            Request::WriteMemory { address, data } => match mock_span(&mut memory, address, data.len()) {
                Some(span) => {
                    span.copy_from_slice(&data);
                    Response::Ok
                }
                None => Response::error(ErrorKind::System, "unmapped"),
            },
            Request::Screenshot | Request::ScreenshotRead { .. } => {
                Response::error(ErrorKind::Unsupported, "the mock has no screen")
            }
            Request::KeepAwake { minutes } => Response::KeepAwake { remaining_s: u64::from(minutes.unwrap_or(0)) * 60 },
            Request::Resources => Response::Resources(proto::Resources {
                limits: vec![proto::ResourceUsage { name: "memory".into(), current: 1 << 20, peak: 1 << 20, limit: 8 << 20 }],
                process_used: 1 << 20,
                process_total: 4 << 20,
            }),
            // The daemon accepts these, then goes away.
            Request::RebootToHekate { .. } | Request::Exit | Request::Handover => {
                stream.write_all(&proto::encode(&Response::Ok)?).await?;
                return Ok(());
            }
            Request::Delete { .. } if upload.is_some() => Response::error(ErrorKind::UploadState, "uploading"),
            Request::Delete { path, recursive } => match path::check_write(&path) {
                Ok(p) => delete(&root.join(&p[1..]), p, recursive),
                Err(e) => Response::error(ErrorKind::Forbidden, e.describe()),
            },
            Request::Profile { duration_ms, interval_ms, thread_id } => {
                let step = interval_ms.clamp(10, 100);
                let ticks = (duration_ms.clamp(50, 10_000) / step).clamp(1, 50);
                let tid = thread_id.unwrap_or(0x5a);
                let samples = (0..ticks)
                    .map(|_| proto::ProfileSample {
                        thread_id: tid,
                        pc: MOCK_NRO + 0x100,
                        lr: MOCK_NRO + 0x204,
                        frames: vec![MOCK_NRO + 0x204, 0x1234],
                    })
                    .collect();
                Response::Profile { app: MOCK_APP, elapsed_ms: duration_ms, ticks, samples }
            }
            Request::AudioCapture => Response::AudioCapture(console.audio_capture()),
            Request::CaptureHold { .. } => Response::AudioCapture(proto::AudioCapture { state: proto::CaptureState::Held, ..console.audio_capture() }),
            Request::AudioReplay { duration_ms } => {
                let c = console.audio_capture();
                let len = if c.state == proto::CaptureState::Capturing {
                    proto::audio::bytes_for_ms(duration_ms.min(proto::audio::REPLAY_MAX_MS)).min(c.cursor)
                } else {
                    0
                };
                audio_replay = Some((c.cursor - len, len));
                Response::Data { data: Vec::new(), file_size: len }
            }
            Request::AudioRead { offset, len } => match audio_replay {
                Some((start, total)) => {
                    let offset = offset.min(total);
                    let len = (len as u64).min(proto::MAX_CHUNK as u64).min(total - offset);
                    Response::Data { data: mock_pcm(start + offset, len as usize, audio_volume, audio_muted), file_size: total }
                }
                None => Response::error(ErrorKind::BadRequest, "no audio replay on this connection"),
            },
            Request::AudioStreamChunk { .. } => Response::error(ErrorKind::Unsupported, "replaced by AudioStream in protocol 30"),
            Request::AudioStream { from, min_ms, max_wait_ms } => {
                let c = console.audio_capture();
                if c.state != proto::CaptureState::Capturing {
                    Response::AudioChunk(proto::AudioChunk { from: c.cursor, data: Vec::new(), skipped: 0, state: c.state })
                } else {
                    let pos = from.unwrap_or(c.cursor);
                    let want = proto::audio::bytes_for_ms(min_ms).min(proto::MAX_CHUNK as u64);
                    let deadline = Instant::now() + std::time::Duration::from_millis(u64::from(max_wait_ms.min(proto::audio::MAX_WAIT_MS)));
                    let mut head = c.cursor;
                    while head.saturating_sub(pos) < want && Instant::now() < deadline {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        head = console.audio_head();
                    }
                    let oldest = head.saturating_sub(proto::audio::bytes_for_ms(proto::audio::RING_MS));
                    let begin = pos.min(head).max(oldest);
                    let len = (head - begin).min(proto::MAX_CHUNK as u64 & !3) as usize;
                    Response::AudioChunk(proto::AudioChunk {
                        from: begin,
                        data: mock_pcm(begin, len, audio_volume, audio_muted),
                        skipped: begin - pos.min(head),
                        state: proto::CaptureState::Capturing,
                    })
                }
            }
            Request::VideoStream { .. } => Response::VideoChunk(proto::VideoChunk {
                data: Vec::new(),
                frame_start: false,
                frame_end: false,
                timestamp: 0,
                dropped: 0,
                state: proto::CaptureState::Unsupported,
            }),
            Request::AudioControl { volume, muted } => {
                if let Some(v) = volume {
                    audio_volume = v.min(100);
                    if v > 0 && muted.is_none() {
                        audio_muted = false;
                    }
                }
                if let Some(m) = muted {
                    audio_muted = m;
                }
                Response::AudioStatus(proto::AudioOutStatus {
                    sample_rate: 48_000,
                    channels: 2,
                    target: "BuiltInSpeaker".into(),
                    volume_pct: if audio_muted { 0 } else { audio_volume },
                    muted: audio_muted,
                    output_active: !audio_muted && audio_volume > 0,
                })
            }
        };
        stream.write_all(&proto::encode(&resp)?).await?;
    }
}

const MOCK_RECORDING: [proto::RecordedInput; 5] = [
    proto::RecordedInput { at_ms: 0, buttons: 0, left: (0, 0), right: (0, 0) },
    proto::RecordedInput { at_ms: 200, buttons: 1, left: (0, 0), right: (0, 0) },
    proto::RecordedInput { at_ms: 300, buttons: 0, left: (0, 0), right: (0, 0) },
    proto::RecordedInput { at_ms: 500, buttons: 0, left: (0, proto::STICK_MAX), right: (0, 0) },
    proto::RecordedInput { at_ms: 1500, buttons: 0, left: (0, 0), right: (0, 0) },
];

/// `len` bytes of the mock's capture from stream position `pos`: a 440 Hz tone, continuous across calls.
fn mock_pcm(pos: u64, len: usize, volume: u8, muted: bool) -> Vec<u8> {
    let amp = if muted || volume == 0 { 0.0 } else { 12_000.0 * (f64::from(volume) / 100.0) };
    let first = pos / 4;
    let mut out = Vec::with_capacity(len);
    for n in 0..(len / 4) as u64 {
        let t = (first + n) as f64 / 48_000.0;
        let s = (amp * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16;
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// [`Request::Find`] over the mock's folder: entries under `sd_dir` (an SD path) whose name contains
/// `needle`, one more than `max` at most.
fn find(root: &Path, sd_dir: &str, needle: &str, max: usize, out: &mut Vec<DirEntry>) {
    let Ok(read) = std::fs::read_dir(root.join(sd_dir.trim_start_matches('/'))) else { return };
    for e in read.filter_map(|e| e.ok()) {
        if out.len() > max {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let full = format!("{}/{name}", sd_dir.trim_end_matches('/'));
        let is_dir = e.path().is_dir();
        if name.to_lowercase().contains(needle) {
            out.push(DirEntry { name: full.clone(), is_dir, size: e.metadata().map_or(0, |m| m.len()) });
        }
        if is_dir {
            find(root, &full, needle, max, out);
        }
    }
}

fn grep(root: &Path, sd_path: &str, needle: &str, ignore_case: bool, max: usize) -> Response {
    let local = root.join(sd_path.trim_start_matches('/'));
    let files: Vec<String> = if local.is_dir() {
        let mut all = Vec::new();
        find(root, sd_path, "", usize::MAX - 1, &mut all);
        all.into_iter().filter(|e| !e.is_dir).map(|e| e.name).collect()
    } else if local.exists() {
        vec![sd_path.to_string()]
    } else {
        return Response::error(ErrorKind::NotFound, format!("stat {sd_path}: not found"));
    };
    let lower = needle.to_lowercase();
    let mut matches = Vec::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(root.join(file.trim_start_matches('/'))) else { continue };
        for (i, line) in text.lines().enumerate() {
            let hit = if ignore_case { line.to_lowercase().contains(&lower) } else { line.contains(needle) };
            if hit {
                if matches.len() == max {
                    return Response::Matches { matches, files: files.len() as u32, truncated: true };
                }
                matches.push(proto::GrepMatch { path: file.clone(), line: i as u32 + 1, text: line.to_string() });
            }
        }
    }
    Response::Matches { matches, files: files.len() as u32, truncated: false }
}

fn io_err(e: std::io::Error) -> Response {
    let kind = if e.kind() == std::io::ErrorKind::NotFound { ErrorKind::NotFound } else { ErrorKind::Io };
    Response::Error(Error { kind, message: e.to_string(), result: None })
}

fn list(dir: &Path) -> Response {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => return io_err(e),
    };
    let mut entries: Vec<DirEntry> = read
        .filter_map(|e| e.ok())
        .map(|e| {
            let meta = e.metadata().ok();
            DirEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                size: meta.map_or(0, |m| if m.is_dir() { 0 } else { m.len() }),
            }
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let truncated = entries.len() > proto::MAX_DIR_ENTRIES;
    entries.truncate(proto::MAX_DIR_ENTRIES);
    Response::Dir { entries, truncated }
}

fn stat(p: &Path) -> Response {
    match std::fs::metadata(p) {
        Ok(m) => Response::Stat(DirEntry {
            name: p.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned()),
            is_dir: m.is_dir(),
            size: if m.is_dir() { 0 } else { m.len() },
        }),
        Err(e) => io_err(e),
    }
}

fn delete(p: &Path, path: String, recursive: bool) -> Response {
    let was_dir = p.is_dir();
    let result = match (was_dir, recursive) {
        (false, _) => std::fs::remove_file(p),
        (true, false) => std::fs::remove_dir(p),
        (true, true) => std::fs::remove_dir_all(p),
    };
    match result {
        Ok(()) => Response::Deleted { path, was_dir },
        Err(e) => io_err(e),
    }
}

fn read(p: &Path, offset: u64, len: u32) -> Response {
    match std::fs::read(p) {
        Ok(all) => {
            let start = (offset as usize).min(all.len());
            let end = (start + (len as usize).min(proto::MAX_CHUNK)).min(all.len());
            Response::Data { data: all[start..end].to_vec(), file_size: all.len() as u64 }
        }
        Err(e) => io_err(e),
    }
}
