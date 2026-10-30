// SPDX-License-Identifier: GPL-3.0-only
//! Async client for the daemon. Keeps one connection open and reconnects when it drops (the daemon
//! closes idle clients after two minutes, and Wi-Fi comes and goes).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use proto::{DaemonKind, DirEntry, HekateTarget, PROTOCOL_VERSION, Request, Response, SysmoduleStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// SD writes of a 64 KiB chunk are quick, but the first write after creating a large file can stall.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Chunks in flight during uploads and reads. One round trip per 64 KiB chunk capped transfers at
/// ~1.5 MB/s over Wi-Fi; keeping several queued overlaps the network with the console's SD access.
const PIPELINE_DEPTH: usize = 4;

pub struct DaemonInfo {
    pub version: String,
    pub kind: DaemonKind,
}

pub struct Client {
    addr: String,
    token: String,
    conn: Mutex<Option<Conn>>,
}

struct Conn {
    stream: TcpStream,
    info: DaemonInfo,
}

#[derive(Debug)]
pub struct UploadStats {
    pub size: u64,
    pub elapsed: Duration,
}

impl Client {
    pub fn new(addr: String, token: String) -> Self {
        Client { addr, token, conn: Mutex::new(None) }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Connects (if needed) and returns what the daemon reported in its Hello.
    pub async fn hello(&self) -> Result<(String, DaemonKind)> {
        let mut guard = self.conn.lock().await;
        let conn = self.connected(&mut guard).await?;
        Ok((conn.info.version.clone(), conn.info.kind))
    }

    /// Returns the round-trip time and the console's uptime.
    pub async fn ping(&self) -> Result<(Duration, Duration)> {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64;
        let start = Instant::now();
        match self.request(Request::Ping { nonce }).await? {
            Response::Pong { nonce: n, uptime_ms } if n == nonce => {
                Ok((start.elapsed(), Duration::from_millis(uptime_ms)))
            }
            other => Err(unexpected(other)),
        }
    }

    pub async fn list_dir(&self, path: &str) -> Result<(Vec<DirEntry>, bool)> {
        match self.request(Request::ListDir { path: path.into() }).await? {
            Response::Dir { entries, truncated } => Ok((entries, truncated)),
            other => Err(unexpected(other)),
        }
    }

    pub async fn stat(&self, path: &str) -> Result<DirEntry> {
        match self.request(Request::Stat { path: path.into() }).await? {
            Response::Stat(entry) => Ok(entry),
            other => Err(unexpected(other)),
        }
    }

    /// Reads up to `len` bytes from `offset`, in as many chunks as needed. Returns the data and the
    /// file's total size.
    pub async fn read(&self, path: &str, offset: u64, len: u64) -> Result<(Vec<u8>, u64)> {
        let read_at = |at: u64, want: u64| Request::ReadFile {
            path: path.into(),
            offset: at,
            len: want.min(proto::MAX_CHUNK as u64) as u32,
        };
        let mut guard = self.conn.lock().await;
        // The first chunk tells us the file's size; the rest are pipelined.
        let (mut out, file_size) = match self.request_locked(&mut guard, read_at(offset, len)).await? {
            Response::Data { data, file_size } => (data, file_size),
            other => return Err(unexpected(other)),
        };
        let end = offset.saturating_add(len).min(file_size);
        let result = async {
            let conn = self.connected(&mut guard).await?;
            let mut next = offset + out.len() as u64;
            let mut in_flight = std::collections::VecDeque::new();
            while next < end || !in_flight.is_empty() {
                while next < end && in_flight.len() < PIPELINE_DEPTH {
                    let want = (end - next).min(proto::MAX_CHUNK as u64);
                    send(conn, &read_at(next, want)).await?;
                    in_flight.push_back(want);
                    next += want;
                }
                let want = in_flight.pop_front().unwrap();
                match recv(conn).await? {
                    Response::Data { data, .. } if data.len() as u64 == want => out.extend_from_slice(&data),
                    Response::Data { .. } => return Err(anyhow!("{path} changed size while being read")),
                    other => return Err(unexpected(other)),
                }
            }
            anyhow::Ok(())
        }
        .await;
        if result.is_err() {
            // Unread replies may still be queued on the connection.
            *guard = None;
        }
        result.map(|()| (out, file_size))
    }

    /// Uploads `data` to `path`, verified with CRC-32 before it replaces the old file.
    pub async fn upload(&self, path: &str, data: &[u8]) -> Result<UploadStats> {
        let start = Instant::now();
        let mut guard = self.conn.lock().await;
        // The whole upload runs on one connection: the daemon's staging state is per connection.
        let begin = Request::UploadBegin { path: path.into(), size: data.len() as u64 };
        expect(self.request_locked(&mut guard, begin).await?, |r| matches!(r, Response::UploadReady { .. }))?;
        let result = async {
            let conn = self.connected(&mut guard).await?;
            let chunks: Vec<&[u8]> = data.chunks(proto::MAX_CHUNK).collect();
            let (mut sent, mut acked, mut offset) = (0, 0, 0u64);
            while acked < chunks.len() {
                while sent < chunks.len() && sent - acked < PIPELINE_DEPTH {
                    send(conn, &Request::UploadChunk { offset, data: chunks[sent].to_vec() }).await?;
                    offset += chunks[sent].len() as u64;
                    sent += 1;
                }
                let received = ((acked + 1) * proto::MAX_CHUNK).min(data.len()) as u64;
                expect(recv(conn).await?, |r| matches!(r, Response::ChunkOk { received: n } if *n == received))?;
                acked += 1;
            }
            let finish = Request::UploadFinish { crc32: proto::crc32::checksum(data) };
            expect(round_trip(conn, &finish).await?, |r| matches!(r, Response::UploadDone { .. }))
        }
        .await;
        if result.is_err() {
            *guard = None;
        }
        result.map(|_| UploadStats { size: data.len() as u64, elapsed: start.elapsed() })
    }

    /// Deletes a file or directory; returns whether it was a directory.
    pub async fn delete(&self, path: &str, recursive: bool) -> Result<bool> {
        match self.request(Request::Delete { path: path.into(), recursive }).await? {
            Response::Deleted { was_dir, .. } => Ok(was_dir),
            other => Err(unexpected(other)),
        }
    }

    /// Installs the sysmodule (`exefs.nsp`) and this client's token, which the sysmodule reads at start.
    /// Doesn't create `boot2.flag`, so it won't start at boot.
    pub async fn install_sysmodule(&self, nsp: &[u8]) -> Result<UploadStats> {
        self.upload(proto::path::TOKEN_PATH, format!("{}\n", self.token).as_bytes()).await?;
        self.upload(proto::path::SYSMODULE_NSP, nsp).await
    }

    /// Turns start-at-boot on or off by creating or deleting the sysmodule's `boot2.flag`. Takes effect at
    /// the next boot; returns the new status.
    pub async fn set_boot2(&self, on: bool) -> Result<SysmoduleStatus> {
        let status = self.sysmodule(Request::SysmoduleStatus).await?;
        if on && !status.installed {
            anyhow::bail!("install the sysmodule before enabling start-at-boot");
        }
        if on && !status.boot2 {
            self.upload(proto::path::SYSMODULE_BOOT2_FLAG, &[]).await?;
        } else if !on && status.boot2 {
            self.delete(proto::path::SYSMODULE_BOOT2_FLAG, false).await?;
        }
        self.sysmodule(Request::SysmoduleStatus).await
    }

    /// Sends a [`Request::SysmoduleStatus`], `SysmoduleStart` or `SysmoduleStop`.
    pub async fn sysmodule(&self, req: Request) -> Result<SysmoduleStatus> {
        match self.request(req).await? {
            Response::Sysmodule(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    /// The daemon's resource limit usage (see [`Request::Resources`]).
    pub async fn resources(&self) -> Result<proto::Resources> {
        match self.request(Request::Resources).await? {
            Response::Resources(r) => Ok(r),
            other => Err(unexpected(other)),
        }
    }

    /// Reads one 4 KiB page of IRAM (sysmodule only; see [`Request::IramPeek`]).
    pub async fn iram_peek(&self, offset: u32) -> Result<Vec<u8>> {
        match self.request(Request::IramPeek { offset }).await? {
            Response::Data { data, .. } => Ok(data),
            other => Err(unexpected(other)),
        }
    }

    /// Presses `buttons` together `times` times on a virtual controller (see [`Request::Press`]).
    pub async fn press(&self, buttons: Vec<proto::Button>, times: u8, hold_ms: u32) -> Result<()> {
        expect(self.request(Request::Press { buttons, times, hold_ms }).await?, |r| matches!(r, Response::Ok)).map(|_| ())
    }

    /// Keeps the console awake for `minutes` (0 ends it; `None` just asks); returns the seconds left.
    pub async fn keep_awake(&self, minutes: Option<u32>) -> Result<u64> {
        match self.request(Request::KeepAwake { minutes }).await? {
            Response::KeepAwake { remaining_s } => Ok(remaining_s),
            other => Err(unexpected(other)),
        }
    }

    /// Asks the NRO daemon to exit. The connection is dropped afterwards.
    pub async fn exit(&self) -> Result<()> {
        let resp = self.request(Request::Exit).await;
        *self.conn.lock().await = None;
        expect(resp?, |r| matches!(r, Response::Ok)).map(|_| ())
    }

    /// Hands the console over from the NRO daemon to the sysmodule (replacing any running one) and waits
    /// until the sysmodule answers. Returns its status. A no-op if the sysmodule is already answering.
    pub async fn handover(&self) -> Result<SysmoduleStatus> {
        if self.hello().await?.1 == DaemonKind::Nro {
            let resp = self.request(Request::Handover).await;
            *self.conn.lock().await = None;
            expect(resp?, |r| matches!(r, Response::Ok))?;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.hello().await {
                Ok((_, DaemonKind::Sysmodule)) => return self.sysmodule(Request::SysmoduleStatus).await,
                Ok((_, DaemonKind::Nro)) => *self.conn.lock().await = None,
                Err(_) => {}
            }
            if Instant::now() > deadline {
                return Err(anyhow!("the sysmodule didn't answer within 30s; see /kickstand/logs/daemon.log"));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// The console's hekate boot entries (see [`proto::hekate::entries`]).
    pub async fn hekate_entries(&self) -> Result<Vec<String>> {
        let (data, _) = self.read(proto::path::HEKATE_INI, 0, 64 * 1024).await?;
        let ini = String::from_utf8_lossy(&data);
        Ok(proto::hekate::entries(&ini).into_iter().map(String::from).collect())
    }

    /// Warm-reboots into hekate. Returns once the daemon has accepted; the console then goes away.
    pub async fn reboot_to_hekate(&self, target: HekateTarget) -> Result<()> {
        let resp = self.request(Request::RebootToHekate { target }).await;
        *self.conn.lock().await = None;
        expect(resp?, |r| matches!(r, Response::Ok)).map(|_| ())
    }

    /// Sends one request, reconnecting and retrying once if the connection had gone stale.
    async fn request(&self, req: Request) -> Result<Response> {
        let mut guard = self.conn.lock().await;
        self.request_locked(&mut guard, req).await
    }

    /// [`Client::request`] with the connection already locked, for multi-request operations.
    async fn request_locked(&self, guard: &mut MutexGuard<'_, Option<Conn>>, req: Request) -> Result<Response> {
        let had_conn = guard.is_some();
        let first = match self.connected(guard).await {
            Ok(conn) => round_trip(conn, &req).await,
            Err(e) => return Err(e),
        };
        match first {
            Ok(resp) => Ok(resp),
            Err(e) if had_conn => {
                **guard = None;
                let conn = self.connected(guard).await.context(format!("after: {e:#}"))?;
                let resp = round_trip(conn, &req).await;
                if resp.is_err() {
                    **guard = None;
                }
                resp
            }
            Err(e) => {
                **guard = None;
                Err(e)
            }
        }
    }

    async fn connected<'a>(&self, slot: &'a mut Option<Conn>) -> Result<&'a mut Conn> {
        if slot.is_none() {
            *slot = Some(self.connect_retrying().await?);
        }
        Ok(slot.as_mut().unwrap())
    }

    /// The console's TCP stack sometimes resets a connection made right after another one closed (seen
    /// with the sysmodule), so a failed Hello is retried a couple of times. Refused connections (no
    /// daemon) and protocol errors fail straight away.
    async fn connect_retrying(&self) -> Result<Conn> {
        let mut attempt = 0;
        loop {
            match self.connect().await {
                Err(e) if attempt < 2 && is_reset(&e) => attempt += 1,
                other => return other,
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn connect(&self) -> Result<Conn> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.addr))
            .await
            .map_err(|_| anyhow!("timed out connecting to the daemon at {}", self.addr))?
            .with_context(|| format!("connecting to the daemon at {}", self.addr))?;
        stream.set_nodelay(true)?;
        let mut conn = Conn { stream, info: DaemonInfo { version: String::new(), kind: DaemonKind::Nro } };
        let hello = Request::Hello { version: PROTOCOL_VERSION, token: self.token.clone() };
        match round_trip(&mut conn, &hello).await? {
            Response::Hello { daemon_version, kind, .. } => {
                conn.info = DaemonInfo { version: daemon_version, kind };
                Ok(conn)
            }
            other => Err(unexpected(other)),
        }
    }
}

async fn round_trip(conn: &mut Conn, req: &Request) -> Result<Response> {
    send(conn, req).await?;
    recv(conn).await
}

async fn send(conn: &mut Conn, req: &Request) -> Result<()> {
    let frame = proto::encode(req)?;
    let io = conn.stream.write_all(&frame);
    timeout(REPLY_TIMEOUT, io).await.map_err(|_| anyhow!("daemon stopped reading for {REPLY_TIMEOUT:?}"))??;
    Ok(())
}

/// Reads the next response. Responses arrive in request order.
async fn recv(conn: &mut Conn) -> Result<Response> {
    let io = async {
        let mut prefix = [0u8; 4];
        conn.stream.read_exact(&mut prefix).await?;
        let len = proto::frame_len(prefix).ok_or_else(|| anyhow!("daemon sent an oversized frame"))?;
        let mut body = vec![0u8; len];
        conn.stream.read_exact(&mut body).await?;
        anyhow::Ok(proto::decode::<Response>(&body)?)
    };
    timeout(REPLY_TIMEOUT, io).await.map_err(|_| anyhow!("daemon did not reply within {REPLY_TIMEOUT:?}"))?
}

fn is_reset(e: &anyhow::Error) -> bool {
    e.chain().filter_map(|c| c.downcast_ref::<std::io::Error>()).any(|io| {
        matches!(io.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe)
    })
}

fn expect(resp: Response, ok: impl Fn(&Response) -> bool) -> Result<Response> {
    if ok(&resp) { Ok(resp) } else { Err(unexpected(resp)) }
}

fn unexpected(resp: Response) -> anyhow::Error {
    match resp {
        Response::Error(e) => match e.result {
            Some(rc) => anyhow!("{:?}: {} (result {:#x})", e.kind, e.message, rc),
            None => anyhow!("{:?}: {}", e.kind, e.message),
        },
        other => anyhow!("unexpected reply from daemon: {other:?}"),
    }
}

/// Turns "menu", "ums", an entry number or an entry name (case-insensitive) into a hekate target.
pub fn parse_hekate_target(spec: &str, entries: &[String]) -> Result<HekateTarget> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("menu") {
        return Ok(HekateTarget::Menu);
    }
    if spec.eq_ignore_ascii_case("ums") {
        return Ok(HekateTarget::Ums);
    }
    let index = match spec.parse::<usize>() {
        Ok(n) => n,
        Err(_) => 1 + entries
            .iter()
            .position(|e| e.eq_ignore_ascii_case(spec))
            .ok_or_else(|| anyhow!("no hekate entry named {spec:?}; entries: {entries:?}"))?,
    };
    if index == 0 || index > entries.len() || index > u8::MAX as usize {
        return Err(anyhow!("hekate entry {index} doesn't exist; entries (from 1): {entries:?}"));
    }
    Ok(HekateTarget::Entry(index as u8))
}

/// Rejects a path the daemon would refuse, before any network round trip.
pub fn check_path(path: &str, write: bool) -> Result<String> {
    let checked = if write { proto::path::check_write(path) } else { proto::path::check_read(path) };
    checked.map_err(|e| anyhow!("{path}: {}", e.describe()))
}

/// One line per resource: `name: current (peak) / limit`, memory in KiB, plus the process's own memory.
pub fn describe_resources(r: &proto::Resources) -> String {
    let mut out = String::new();
    for u in &r.limits {
        let (cur, peak, lim) = if u.name == "memory" {
            (u.current / 1024, u.peak / 1024, u.limit / 1024)
        } else {
            (u.current, u.peak, u.limit)
        };
        let unit = if u.name == "memory" { " KiB" } else { "" };
        out.push_str(&format!("{}: {cur}{unit} (peak {peak}) of {lim}, {} left\n", u.name, lim - cur));
    }
    out.push_str(&format!("this process: {} of {} KiB\n", r.process_used / 1024, r.process_total / 1024));
    out
}

/// Parses buttons written like "a", "zl+zr" or "l,r" (case-insensitive).
pub fn parse_buttons(spec: &str) -> Result<Vec<proto::Button>> {
    let buttons = spec
        .split(['+', ',', ' '])
        .filter(|s| !s.is_empty())
        .map(|name| {
            proto::Button::from_name(name).ok_or_else(|| {
                let names: Vec<_> = proto::Button::ALL.iter().map(|b| b.name()).collect();
                anyhow!("unknown button {name:?}; buttons are {}", names.join(", "))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if buttons.is_empty() {
        anyhow::bail!("no buttons given");
    }
    Ok(buttons)
}
