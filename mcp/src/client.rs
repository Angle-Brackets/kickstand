// SPDX-License-Identifier: GPL-3.0-only
//! Async client for the daemon. Keeps one connection open and reconnects when it drops (the daemon
//! closes idle clients after two minutes, and Wi-Fi comes and goes).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
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

#[derive(Clone)]
pub struct DaemonInfo {
    pub version: String,
    /// The daemon's [`PROTOCOL_VERSION`].
    pub protocol: u32,
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

    /// The console's address without the port.
    pub fn host(&self) -> &str {
        self.addr.rsplit_once(':').map_or(&self.addr, |(host, _)| host)
    }

    /// Connects (if needed) and returns what the daemon reported in its Hello.
    pub async fn hello(&self) -> Result<DaemonInfo> {
        let mut guard = self.conn.lock().await;
        let conn = self.connected(&mut guard).await?;
        Ok(conn.info.clone())
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
        self.read_chunks(read_at(offset, len), read_at, offset, len, path).await
    }

    /// Captures the screen; returns the JPEG (the kickstand indicator isn't in it).
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let read_at = |at: u64, want: u64| Request::ScreenshotRead {
            offset: at,
            len: want.min(proto::MAX_CHUNK as u64) as u32,
        };
        // caps:sc refuses (2206-0112) while the foreground changes, e.g. an application starting; it
        // passes within a second or two.
        let mut tries = 0;
        loop {
            // One connection for the capture and its reads: the daemon keeps the JPEG per connection.
            match self.read_chunks(Request::Screenshot, read_at, 0, u64::MAX, "the screenshot").await {
                Ok((jpeg, _)) => return Ok(jpeg),
                Err(e) if tries < 4 && format!("{e:#}").contains("caps:sc") => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(700)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Sends `first` (which answers with the first chunk and the total size), then pipelines `read_at`
    /// requests for the rest of `offset..offset + len`. `what` names the data in errors.
    async fn read_chunks(
        &self,
        first: Request,
        read_at: impl Fn(u64, u64) -> Request,
        offset: u64,
        len: u64,
        what: &str,
    ) -> Result<(Vec<u8>, u64)> {
        let mut guard = self.conn.lock().await;
        // The first chunk tells us the total size; the rest are pipelined.
        let (mut out, file_size) = match self.request_locked(&mut guard, first).await? {
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
                    Response::Data { .. } => return Err(anyhow!("{what} changed size while being read")),
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

    /// Plays timed button steps on one virtual controller ([`Request::Input`]).
    pub async fn input(&self, steps: Vec<proto::InputStep>) -> Result<()> {
        expect(self.request(Request::Input { steps }).await?, |r| matches!(r, Response::Ok)).map(|_| ())
    }

    /// The running application, if any.
    pub async fn app_status(&self) -> Result<Option<proto::App>> {
        match self.request(Request::AppStatus).await? {
            Response::App(app) => Ok(app),
            other => Err(unexpected(other)),
        }
    }

    /// Terminates the running application; returns what was closed (`None`: nothing was running).
    pub async fn close_app(&self) -> Result<Option<proto::App>> {
        match self.request(Request::CloseApp).await? {
            Response::App(app) => Ok(app),
            other => Err(unexpected(other)),
        }
    }

    /// Where the console is: uptime, unlock presses, keep-awake, the application and the applet. A daemon
    /// older than protocol 19 is asked the pieces it knows instead.
    pub async fn status(&self) -> Result<proto::ConsoleStatus> {
        match self.request(Request::Status).await? {
            Response::Status(status) => Ok(status),
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => {
                let (_, uptime) = self.ping().await?;
                Ok(proto::ConsoleStatus {
                    uptime_ms: uptime.as_millis() as u64,
                    unlocking_ms: None,
                    keep_awake_s: self.keep_awake(None).await?,
                    application: self.app_status().await?,
                    applet: self.applet_status().await.unwrap_or(None),
                })
            }
            other => Err(unexpected(other)),
        }
    }

    /// The Album's process (hbmenu in applet mode), if it's open.
    pub async fn applet_status(&self) -> Result<Option<proto::App>> {
        match self.request(Request::AppletStatus).await? {
            Response::App(app) => Ok(app),
            other => Err(unexpected(other)),
        }
    }

    /// Terminates the Album's process; returns what was closed (`None`: it wasn't open).
    pub async fn close_applet(&self) -> Result<Option<proto::App>> {
        match self.request(Request::CloseApplet).await? {
            Response::App(app) => Ok(app),
            other => Err(unexpected(other)),
        }
    }

    /// What the console's fatal error screen is about, if it's on one: the newest Atmosphère fatal report
    /// since the daemon started, with what to do. `None` too from a daemon older than protocol 20.
    pub async fn fatal(&self) -> Result<Option<String>> {
        let entries = match self.request(Request::FatalReports).await? {
            Response::Dir { entries, .. } => entries,
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => return Ok(None),
            other => return Err(unexpected(other)),
        };
        let Some(newest) = entries.last() else { return Ok(None) };
        let path = format!("/atmosphere/fatal_reports/{}", newest.name);
        let (data, _) = self.read(&path, 0, 1024).await?;
        let text = String::from_utf8_lossy(&data);
        let field = |name: &str| text.lines().find_map(|l| l.trim().strip_prefix(name)).map_or("?", str::trim).to_string();
        Ok(Some(format!(
            "FATAL ERROR: {} ({}) crashed with {}. The console is stuck on Atmosphère's error screen until it \
             reboots (screenshots fail meanwhile): reboot into hekate (the reboot tool, or `kickstand reboot 1 --yes`), \
             and the sysmodule gets past the lock screen afterwards. Report: {path}",
            field("Process Name:"),
            field("Program ID:"),
            field("Result:")
        )))
    }

    /// Arms the daemon's entry in Atmosphère's override config so launching `program_id` starts hbloader
    /// without R held (0 disarms it). `false` if the daemon predates it (protocol 20).
    pub async fn hbl_override(&self, program_id: u64) -> Result<bool> {
        match self.request(Request::HblOverride { program_id }).await? {
            Response::Ok => Ok(true),
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => Ok(false),
            other => Err(unexpected(other)),
        }
    }

    /// Points this connection's memory requests at the Album's process (`applet`) or the application.
    pub async fn memory_target(&self, applet: bool) -> Result<()> {
        expect(self.request(Request::MemoryTarget { applet }).await?, |r| matches!(r, Response::Ok)).map(|_| ())
    }

    /// Installed applications, most recently used first (the home menu's order).
    pub async fn list_apps(&self, max: u16) -> Result<Vec<proto::AppEntry>> {
        match self.request(Request::ListApps { max }).await? {
            Response::Apps(apps) => Ok(apps),
            other => Err(unexpected(other)),
        }
    }

    /// The running application's memory map.
    pub async fn memory_map(&self) -> Result<proto::MemoryMap> {
        match self.request(Request::MemoryMap).await? {
            Response::Memory(map) => Ok(map),
            other => Err(unexpected(other)),
        }
    }

    /// `len` bytes of the running application's memory at `address`, a chunk per request.
    pub async fn read_memory(&self, address: u64, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let at = address.wrapping_add(out.len() as u64);
            let want = (len - out.len()).min(proto::MAX_CHUNK) as u32;
            match self.request(Request::ReadMemory { address: at, len: want }).await? {
                Response::Data { data, .. } if data.len() == want as usize => out.extend_from_slice(&data),
                Response::Data { data, .. } => bail!("asked for {want} bytes at {at:#x}, got {}", data.len()),
                other => return Err(unexpected(other)),
            }
        }
        Ok(out)
    }

    /// Writes `data` to the running application's memory at `address` (homebrew only).
    pub async fn write_memory(&self, address: u64, data: &[u8]) -> Result<()> {
        for (i, chunk) in data.chunks(proto::MAX_CHUNK).enumerate() {
            let req = Request::WriteMemory { address: address.wrapping_add((i * proto::MAX_CHUNK) as u64), data: chunk.to_vec() };
            expect(self.request(req).await?, |r| matches!(r, Response::Ok))?;
        }
        Ok(())
    }

    /// Moves the sysmodule's indicator (debug; see [`Request::IndicatorPosition`]).
    pub async fn indicator_position(&self, x: u16, y: u16) -> Result<()> {
        expect(self.request(Request::IndicatorPosition { x, y }).await?, |r| matches!(r, Response::Ok)).map(|_| ())
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
        if self.hello().await?.kind == DaemonKind::Nro {
            let resp = self.request(Request::Handover).await;
            *self.conn.lock().await = None;
            expect(resp?, |r| matches!(r, Response::Ok))?;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.hello().await.map(|info| info.kind) {
                Ok(DaemonKind::Sysmodule) => return self.sysmodule(Request::SysmoduleStatus).await,
                Ok(DaemonKind::Nro) => *self.conn.lock().await = None,
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
        let mut conn = Conn { stream, info: DaemonInfo { version: String::new(), protocol: 0, kind: DaemonKind::Nro } };
        let hello = Request::Hello { version: PROTOCOL_VERSION, token: self.token.clone() };
        match round_trip(&mut conn, &hello).await? {
            Response::Hello { version, .. } if !proto::compatible(version) => Err(anyhow!(
                "the daemon speaks protocol {version}, older than {} (the oldest this client talks to): install a newer sysmodule",
                proto::MIN_COMPATIBLE_VERSION
            )),
            Response::Hello { version, daemon_version, kind } => {
                conn.info = DaemonInfo { version: daemon_version, protocol: version, kind };
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
        let reply = proto::decode::<Response>(&body);
        // Only a reply appended after this client's version would fail here.
        anyhow::Ok(reply.with_context(|| format!("couldn't read the daemon's reply (daemon protocol {}, this client {PROTOCOL_VERSION}): update the MCP server", conn.info.protocol))?)
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
/// Parses input steps like `"r:300 r+a:150 r:6000 -:500 l=0,1+b:2000 touch=640,360:100"`: what is held
/// together, joined by `+` (buttons; `l=X,Y` / `r=X,Y` for the sticks, -1.0 to 1.0 with y up;
/// `touch=X,Y` for a finger on the 1280x720 touch screen; `-` for nothing, a pause), then `:` and for how
/// many milliseconds.
pub fn parse_steps(spec: &str) -> Result<Vec<proto::InputStep>> {
    spec.split_whitespace()
        .map(|step| {
            let (held, ms) = step.rsplit_once(':').ok_or_else(|| anyhow!("step {step:?} isn't WHAT:MS"))?;
            let ms = ms.parse().with_context(|| format!("step {step:?}: bad milliseconds"))?;
            let mut parsed = proto::InputStep { ms, ..Default::default() };
            for part in held.split('+').filter(|p| !p.is_empty() && *p != "-") {
                match part.split_once('=') {
                    Some(("l" | "ls", v)) => parsed.left = Some(parse_stick(v)?),
                    Some(("r" | "rs", v)) => parsed.right = Some(parse_stick(v)?),
                    Some(("touch" | "t", v)) => parsed.touch = Some(parse_point(v)?),
                    Some((k, _)) => anyhow::bail!("step {step:?}: unknown {k:?}= (l=, r= or touch=)"),
                    None => parsed.buttons.extend(parse_buttons(part)?),
                }
            }
            Ok(parsed)
        })
        .collect()
}

/// `0,1` (straight up) or `-0.5,0` → stick units.
fn parse_stick(v: &str) -> Result<(i32, i32)> {
    let (x, y) = v.split_once(',').ok_or_else(|| anyhow!("stick {v:?} isn't X,Y"))?;
    let axis = |a: &str| -> Result<i32> {
        let f: f64 = a.trim().parse().with_context(|| format!("stick axis {a:?}"))?;
        if !(-1.0..=1.0).contains(&f) {
            anyhow::bail!("stick axis {a:?} is outside -1..1");
        }
        Ok((f * f64::from(proto::STICK_MAX)).round() as i32)
    };
    Ok((axis(x)?, axis(y)?))
}

/// `640,360` → a touch screen point (0-1279, 0-719).
fn parse_point(v: &str) -> Result<(u16, u16)> {
    let (x, y) = v.split_once(',').ok_or_else(|| anyhow!("touch point {v:?} isn't X,Y"))?;
    let (x, y): (u16, u16) = (x.trim().parse()?, y.trim().parse()?);
    if x > 1279 || y > 719 {
        anyhow::bail!("touch point {v:?} is off the 1280x720 screen");
    }
    Ok((x, y))
}

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

/// `status` as a few lines of text.
pub fn describe_status(status: &proto::ConsoleStatus) -> String {
    let secs = status.uptime_ms / 1000;
    let unlock = match status.unlocking_ms {
        Some(ms) => format!("the sysmodule is still pressing B to get past the lock screen (after an agent reboot) for {} s more", ms.div_ceil(1000)),
        None => "no unlock presses pending".to_string(),
    };
    let awake = match status.keep_awake_s {
        0 => "keep-awake: off (the console's own sleep timers apply)".to_string(),
        s => format!("keep-awake: {} min left", s.div_ceil(60)),
    };
    let application = match &status.application {
        Some(app) => format!("application: {}", describe_app(app)),
        None => "application: none".to_string(),
    };
    let applet = match &status.applet {
        Some(a) if a.hbl == Some(true) => format!("applet mode: hbloader (hbmenu or an NRO) in the Album, pid {}", a.pid),
        Some(a) => format!("applet mode: the Album is open showing pictures, pid {}", a.pid),
        None => "applet mode: the Album isn't open".to_string(),
    };
    format!("uptime {}m {}s; {unlock}\n{awake}\n{application}\n{applet}", secs / 60, secs % 60)
}

pub fn describe_app(app: &proto::App) -> String {
    let what = match app.hbl {
        Some(true) => "hbloader (hbmenu or an NRO) in place of",
        Some(false) => "title",
        None => "title (unknown whether hbloader is in its place)",
    };
    format!("{what} {:016x}, pid {}", app.program_id, app.pid)
}
