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
        Console { root, tokens: Mutex::new(tokens), pair_code, activity: Mutex::default() }
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

/// Answers discovery probes on `socket` like the daemon, pointing at TCP `port`.
pub async fn answer_probes(socket: tokio::net::UdpSocket, port: u16, pairing: bool) -> Result<()> {
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
            nickname: "Mock Switch".into(),
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

async fn handle(mut stream: TcpStream, console: &Console, id: u64) -> Result<()> {
    let root = console.root.as_path();
    let mut authed = false;
    // The token this connection presented.
    let mut session_token = None;
    let mut upload: Option<Upload> = None;
    let mut memory = mock_memory();
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
        if authed && !matches!(req, Request::Activity | Request::Agents | Request::Introduce { .. }) {
            console.request(id, &req);
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
            Request::ListDir { path } => match path::check_read(&path) {
                Ok(p) => list(&root.join(&p[1..])),
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
            Request::Press { .. } | Request::Input { .. } | Request::Hold { .. } | Request::HblOverride { .. } => Response::Ok,
            Request::FatalReports => Response::Dir { entries: Vec::new(), truncated: false },
            Request::AppStatus | Request::CloseApp => Response::App(None),
            Request::ListApps { .. } => Response::Apps(Vec::new()),
            Request::AppletStatus | Request::CloseApplet => Response::App(None),
            Request::MemoryTarget { .. } => Response::Ok,
            Request::Status => Response::Status(proto::ConsoleStatus {
                uptime_ms: 1,
                unlocking_ms: None,
                keep_awake_s: 0,
                application: None,
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
            Request::IndicatorPosition { .. } => Response::Ok,
            Request::Screenshot | Request::ScreenshotRead { .. } => {
                Response::error(ErrorKind::Unsupported, "the mock has no screen")
            }
            Request::KeepAwake { minutes } => Response::KeepAwake { remaining_s: u64::from(minutes.unwrap_or(0)) * 60 },
            Request::Resources => Response::Resources(proto::Resources {
                limits: vec![proto::ResourceUsage { name: "memory".into(), current: 1 << 20, peak: 1 << 20, limit: 8 << 20 }],
                process_used: 1 << 20,
                process_total: 4 << 20,
            }),
            Request::RebootToHekate { .. } => {
                stream.write_all(&proto::encode(&Response::Ok)?).await?;
                return Ok(());
            }
            Request::Exit | Request::Handover => {
                stream.write_all(&proto::encode(&Response::Ok)?).await?;
                return Ok(());
            }
            Request::Delete { .. } if upload.is_some() => Response::error(ErrorKind::UploadState, "uploading"),
            Request::Delete { path, recursive } => match path::check_write(&path) {
                Ok(p) => delete(&root.join(&p[1..]), p, recursive),
                Err(e) => Response::error(ErrorKind::Forbidden, e.describe()),
            },
        };
        stream.write_all(&proto::encode(&resp)?).await?;
    }
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
