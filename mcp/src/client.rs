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

#[derive(Clone, Debug)]
pub struct DaemonInfo {
    pub version: String,
    /// The daemon's [`PROTOCOL_VERSION`].
    pub protocol: u32,
    pub kind: DaemonKind,
}

pub struct Client {
    addr: String,
    token: String,
    /// Talk over the console's USB interface with this serial number ([`Client::over_usb`]) instead of TCP.
    usb: Option<String>,
    /// The AI app this client works for, told to the console on each connection ([`Request::Introduce`]).
    app: std::sync::Mutex<Option<String>>,
    conn: Mutex<Option<Conn>>,
}

struct Conn {
    stream: Stream,
    info: DaemonInfo,
    /// Requests sent (or being sent) whose replies haven't been read in full. Above 0 between calls, a
    /// call was cancelled part way (an agent cancelling a tool mid-upload): a frame may be half sent and
    /// replies are queued, so the connection is out of step and [`Client::connected`] replaces it.
    pending: u32,
}

enum Stream {
    Tcp(TcpStream),
    Usb(Box<crate::usb::UsbLink>),
}

/// How a [`Client::play`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Played {
    Finished,
    /// Stopped with [`Client::playback`] (by anyone), or paused longer than [`proto::MAX_PAUSE_MS`].
    Stopped,
}

#[derive(Debug)]
pub struct UploadStats {
    pub size: u64,
    pub elapsed: Duration,
}

/// Who a connection is for, as [`Request::Introduce`] tells the console: the AI app's name, then a tag for
/// this process ([`session_tag`]), so the desktop app can tell one agent's reconnections from a new agent.
/// The console keeps the text as it is ([`proto::MAX_APP_NAME`] bytes at most).
const SESSION_SEP: char = '\u{1f}';
/// What `kickstand <command>` in a terminal (or a script) introduces itself as.
pub const TERMINAL: &str = "kickstand-cli";
/// What Kickstand's desktop app introduces itself as.
pub const DESKTOP: &str = "kickstand-desktop";

/// This process's tag: the same for every connection it makes, different for every process.
pub fn session_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        format!("{:06x}", (std::process::id() ^ nanos.rotate_left(13)) & 0xFF_FFFF)
    })
}

/// `app` with this process's tag, cut to fit.
pub fn introduction(app: &str) -> String {
    let tag = session_tag();
    let mut end = app.len().min(proto::MAX_APP_NAME - tag.len() - SESSION_SEP.len_utf8());
    while !app.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{SESSION_SEP}{tag}", &app[..end])
}

/// An introduction's app name and session tag (none from Kickstand before session tags).
pub fn split_introduction(text: &str) -> (&str, Option<&str>) {
    match text.split_once(SESSION_SEP) {
        Some((app, tag)) => (app, Some(tag)),
        None => (text, None),
    }
}

/// Whether an introduction is a person's (a terminal command, or the desktop app), not an agent's.
pub fn is_person(text: &str) -> bool {
    matches!(split_introduction(text).0, TERMINAL | DESKTOP)
}

/// What an unreachable console most likely means.
const ASLEEP: &str = "the console doesn't answer on the network: it's most likely asleep (it can't be woken remotely), \
    turned off, or off Wi-Fi. Ask the user to wake and unlock it rather than retrying in a loop";

impl Client {
    pub fn new(addr: String, token: String) -> Self {
        Client { addr, token, usb: None, app: std::sync::Mutex::new(None), conn: Mutex::new(None) }
    }

    /// A client for the same console over its USB interface (serial number from [`Client::usb_open`]).
    pub fn over_usb(&self, serial: String) -> Client {
        let mut other = self.another();
        other.usb = Some(serial);
        other
    }

    /// Asks the console to present its USB interface ([`Request::UsbOpen`]); returns its serial number.
    pub async fn usb_open(&self) -> Result<String> {
        match self.request(Request::UsbOpen).await? {
            Response::UsbOpened { serial } => Ok(serial),
            other => Err(too_old_for(other, "USB transfers", 28)),
        }
    }

    /// Ends the console's USB interface ([`Request::UsbClose`]), ignoring failures; a client over USB drops
    /// its connection too.
    pub async fn usb_close(&self) {
        let mut guard = self.conn.lock().await;
        if let Some(conn) = guard.as_mut() {
            let _ = round_trip(conn, &Request::UsbClose).await;
        } else if self.usb.is_none() {
            let _ = self.request_locked(&mut guard, Request::UsbClose).await;
        }
        if self.usb.is_some() {
            *guard = None;
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// A client for the same console with a connection of its own, for work alongside this one's (frames
    /// captured while this one holds buttons).
    pub fn another(&self) -> Client {
        let mut other = Client::new(self.addr.clone(), self.token.clone());
        other.usb = self.usb.clone();
        *other.app.lock().unwrap() = self.app();
        other
    }

    /// This client, introducing itself as `app` on every connection (for clients named before they connect).
    pub fn named(self, app: &str) -> Self {
        *self.app.lock().unwrap() = Some(app.to_string());
        self
    }

    pub fn app(&self) -> Option<String> {
        self.app.lock().unwrap().clone()
    }

    /// Names the AI app this client works for, telling the console now if connected (and on every
    /// connection after).
    pub async fn set_app(&self, app: &str) {
        if self.app.lock().unwrap().replace(app.to_string()).as_deref() == Some(app) {
            return;
        }
        if let Some(conn) = self.conn.lock().await.as_mut() {
            // An older console doesn't know the request; nothing to do about it.
            round_trip(conn, &Request::Introduce { app: introduction(app) }).await.ok();
        }
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
                Err(e) if format!("{e:#}").contains("caps:sc") => {
                    if tries == 4 {
                        return Err(e.context(
                            "the console kept refusing screenshots for 3 s (it does while the foreground changes, and it \
                             can stay that way while some applets are in front, like the Album running hbmenu)",
                        ));
                    }
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

    /// Streams `local` from disk to `path` in [`proto::MAX_CHUNK`] pieces, verified with CRC-32 before it
    /// replaces the old file, without reading the whole file into memory.
    pub async fn upload_file(&self, path: &str, local: &std::path::Path) -> Result<UploadStats> {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(local).await.with_context(|| format!("opening {}", local.display()))?;
        let meta = file.metadata().await.with_context(|| format!("reading {}", local.display()))?;
        anyhow::ensure!(meta.is_file(), "{} isn't a regular file", local.display());
        let size = meta.len();
        let start = Instant::now();
        let mut guard = self.conn.lock().await;
        // The whole upload runs on one connection: the daemon's staging state is per connection.
        let begin = Request::UploadBegin { path: path.into(), size };
        expect(self.request_locked(&mut guard, begin).await?, |r| matches!(r, Response::UploadReady { .. }))?;
        let result = async {
            let conn = self.connected(&mut guard).await?;
            let mut crc = proto::crc32::INIT;
            let (mut sent_bytes, mut acked_bytes) = (0u64, 0u64);
            let mut in_flight = std::collections::VecDeque::new();
            while acked_bytes < size {
                while sent_bytes < size && in_flight.len() < PIPELINE_DEPTH {
                    let want = (size - sent_bytes).min(proto::MAX_CHUNK as u64) as usize;
                    let mut buf = vec![0u8; want];
                    file.read_exact(&mut buf).await.with_context(|| format!("reading {}", local.display()))?;
                    crc = proto::crc32::update(crc, &buf);
                    let offset = sent_bytes;
                    sent_bytes += want as u64;
                    send(conn, &Request::UploadChunk { offset, data: buf }).await?;
                    in_flight.push_back(sent_bytes);
                }
                let expected = in_flight.pop_front().unwrap();
                expect(recv(conn).await?, |r| matches!(r, Response::ChunkOk { received: n } if *n == expected))?;
                acked_bytes = expected;
            }
            let finish = Request::UploadFinish { crc32: proto::crc32::finish(crc) };
            expect(round_trip(conn, &finish).await?, |r| matches!(r, Response::UploadDone { .. }))
        }
        .await;
        if result.is_err() {
            *guard = None;
        }
        result.map(|_| UploadStats { size, elapsed: start.elapsed() })
    }

    /// Deletes a file or directory; returns whether it was a directory.
    pub async fn delete(&self, path: &str, recursive: bool) -> Result<bool> {
        match self.request(Request::Delete { path: path.into(), recursive }).await? {
            Response::Deleted { was_dir, .. } => Ok(was_dir),
            other => Err(unexpected(other)),
        }
    }

    /// Installs the sysmodule (`exefs.nsp`) and makes sure the token file, which it reads at start, has this
    /// client's token (other paired PCs' tokens stay). Doesn't create `boot2.flag`, so it won't start at boot.
    pub async fn install_sysmodule(&self, nsp: &[u8]) -> Result<UploadStats> {
        let path = proto::path::TOKEN_PATH;
        let existing = match self.read(path, 0, 16 * 1024).await {
            Ok((data, _)) => String::from_utf8(data).with_context(|| format!("{path} isn't text"))?,
            Err(e) if format!("{e:#}").contains("NotFound") => String::new(),
            Err(e) => return Err(e.context(format!("reading {path}"))),
        };
        // Tokens older than the pairing rules are kept as they were.
        let updated = match proto::tokens::add(&existing, &self.token, "installed the sysmodule") {
            Ok(updated) => updated,
            Err(_) if proto::tokens::parse(&existing).contains(&self.token.as_str()) => None,
            Err(e) => bail!("can't add this PC's token to {path}: {e:?}"),
        };
        if let Some(text) = updated {
            self.upload(path, text.as_bytes()).await?;
        }
        self.upload(proto::path::SYSMODULE_NSP, nsp).await
    }

    /// Turns start-at-boot on or off by creating or deleting the sysmodule's `boot2.flag`. Takes effect at
    /// the next boot; returns the new status.
    pub async fn set_boot2(&self, on: bool) -> Result<SysmoduleStatus> {
        let status = self.sysmodule(Request::SysmoduleStatus).await?;
        if on && !status.installed {
            bail!("install the sysmodule before enabling start-at-boot");
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
        expect_ok(self.request(Request::Press { buttons, times, hold_ms }).await?)
    }

    /// Plays timed button steps on one virtual controller ([`Request::Input`]).
    pub async fn input(&self, steps: Vec<proto::InputStep>) -> Result<()> {
        expect_ok(self.request(Request::Input { steps }).await?)
    }

    /// Holds `step` for `step.ms` on a controller that stays attached meanwhile (see [`Request::Hold`]);
    /// `false` from a daemon older than protocol 23.
    pub async fn hold(&self, step: proto::InputStep) -> Result<bool> {
        match self.request(Request::Hold { step }).await? {
            Response::Ok => Ok(true),
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => Ok(false),
            other => Err(unexpected(other)),
        }
    }

    /// Plays `chunks` back to back on a held controller (see [`Request::Play`]), sending each next one
    /// while the previous plays so nothing is let go between them, then lets go.
    pub async fn play(&self, chunks: Vec<Vec<proto::InputStep>>) -> Result<Played> {
        /// How long the last step of a chunk is held while the next one is on its way.
        const LEASE_MS: u32 = 3_000;
        // A paused macro's chunk is answered once resumed.
        let wait = REPLY_TIMEOUT + Duration::from_millis(u64::from(proto::MAX_PAUSE_MS));
        let mut guard = self.conn.lock().await;
        let result = async {
            let conn = self.connected(&mut guard).await?;
            let mut chunks = chunks.into_iter();
            let mut in_flight = 0;
            let mut played = Played::Finished;
            loop {
                while in_flight < 2 && played == Played::Finished {
                    let Some(steps) = chunks.next() else { break };
                    send(conn, &Request::Play { steps, lease_ms: LEASE_MS }).await?;
                    in_flight += 1;
                }
                if in_flight == 0 {
                    break;
                }
                // After a stop, the chunk still on its way is refused at once.
                match recv_within(conn, wait).await? {
                    Response::Ok => in_flight -= 1,
                    Response::Playback(_) => {
                        in_flight -= 1;
                        played = Played::Stopped;
                    }
                    other => return Err(too_old(other, "replaying recordings")),
                }
            }
            anyhow::Ok(played)
        }
        .await;
        if result.is_err() {
            // Unread replies may still be queued on the connection.
            *guard = None;
        }
        drop(guard);
        // Let go whether it played or not.
        let _ = self.hold(proto::InputStep::default()).await;
        result
    }

    /// Pauses, resumes or stops the macro playing on the console, whoever plays it (or just asks, with
    /// `Query`), and returns how things were before. [`Client::play`] holds the connection while it plays,
    /// so control a macro this client plays from [`Client::another`].
    pub async fn playback(&self, action: proto::PlaybackAction) -> Result<proto::Playback> {
        match self.request(Request::Playback { action }).await? {
            Response::Playback(state) => Ok(state),
            other => Err(too_old_for(other, "pausing and stopping macros", 27)),
        }
    }

    /// Starts recording the physical controllers for at most `max_ms`.
    pub async fn record_start(&self, max_ms: u32) -> Result<()> {
        match self.request(Request::RecordStart { max_ms }).await? {
            Response::Ok => Ok(()),
            other => Err(too_old(other, "recording")),
        }
    }

    /// Stops recording and fetches what was recorded; also how long it ran (ms) and whether it filled up.
    pub async fn record_stop(&self) -> Result<(Vec<proto::RecordedInput>, u32, bool)> {
        let (total, ms, full) = match self.request(Request::RecordStop).await? {
            Response::Recording { total, ms, full } => (total, ms, full),
            other => return Err(too_old(other, "recording")),
        };
        let mut inputs = Vec::with_capacity(total as usize);
        while inputs.len() < total as usize {
            match self.request(Request::RecordRead { from: inputs.len() as u32 }).await? {
                Response::Recorded { inputs: page } if !page.is_empty() => inputs.extend(page),
                Response::Recorded { .. } => break,
                other => return Err(unexpected(other)),
            }
        }
        Ok((inputs, ms, full))
    }

    /// Types `text` on a virtual keyboard.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        if let Some(c) = text.chars().find(|c| !(c.is_ascii_graphic() || matches!(c, ' ' | '\n' | '\t'))) {
            bail!("can't type {c:?}: printable ASCII, tabs and newlines only (a US keyboard)");
        }
        if text.chars().count() > proto::MAX_TYPE_TEXT {
            bail!("at most {} characters at a time; split it up", proto::MAX_TYPE_TEXT);
        }
        match self.request(Request::TypeText { text: text.into() }).await? {
            Response::Ok => Ok(()),
            other => Err(too_old(other, "typing")),
        }
    }

    /// Entries under `dir` whose name contains `name`, searched on the console.
    pub async fn find(&self, dir: &str, name: &str, max: u32) -> Result<(Vec<DirEntry>, bool)> {
        match self.request(Request::Find { dir: dir.into(), name: name.into(), max }).await? {
            Response::Found { entries, truncated } => Ok((entries, truncated)),
            other => Err(too_old(other, "searching on the console")),
        }
    }

    /// Lines containing `needle` in a file or the files under a folder, searched on the console; also how
    /// many files were searched and whether it stopped early.
    pub async fn grep(&self, path: &str, needle: &str, ignore_case: bool, max: u32) -> Result<(Vec<proto::GrepMatch>, u32, bool)> {
        match self.request(Request::Grep { path: path.into(), needle: needle.into(), ignore_case, max }).await? {
            Response::Matches { matches, files, truncated } => Ok((matches, files, truncated)),
            other => Err(too_old(other, "searching on the console")),
        }
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
        expect_ok(self.request(Request::MemoryTarget { applet }).await?)
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
            expect_ok(self.request(req).await?)?;
        }
        Ok(())
    }

    /// Moves the sysmodule's indicator (debug; see [`Request::IndicatorPosition`]).
    pub async fn indicator_position(&self, x: u16, y: u16) -> Result<()> {
        expect_ok(self.request(Request::IndicatorPosition { x, y }).await?)
    }

    /// Who is connected and what they've been doing (see [`Request::Activity`]); `None` from a daemon older
    /// than protocol 22. Doesn't count as activity or keep the console awake.
    pub async fn activity(&self) -> Result<Option<proto::ActivityReport>> {
        match self.request(Request::Activity).await? {
            Response::Activity(report) => Ok(Some(report)),
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => Ok(None),
            other => Err(unexpected(other)),
        }
    }

    /// Every connected client: its app, what it's doing and the last thing it changed, and what clients
    /// did lately (see [`Request::Agents`]); `None` before protocol 22. Before 24 (from
    /// [`Request::Activity`]), apps and ids (0) aren't known, and the last change is known only while
    /// it's what the client is doing.
    pub async fn agents(&self) -> Result<Option<proto::AgentsReport>> {
        match self.request(Request::Agents).await? {
            Response::Agents(report) => return Ok(Some(report)),
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => {}
            other => return Err(unexpected(other)),
        }
        let Some(report) = self.activity().await? else { return Ok(None) };
        let agents = report
            .clients
            .into_iter()
            .map(|c| {
                let change = proto::changes_console(&c.doing).then(|| (c.doing.clone(), c.idle_ms));
                proto::AgentInfo { id: 0, ip: c.ip, you: c.you, app: None, connected_ms: c.connected_ms, requests: c.requests, doing: c.doing, idle_ms: c.idle_ms, change }
            })
            .collect();
        let recent = report.recent.into_iter().map(|e| proto::AgentEvent { ago_ms: e.ago_ms, id: 0, ip: e.ip, app: None, what: e.what }).collect();
        Ok(Some(proto::AgentsReport { agents, recent }))
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
        self.disconnect().await;
        expect_ok(resp?)
    }

    /// Every thread of the memory target (see [`Client::memory_target`]), paused for a moment.
    pub async fn threads(&self) -> Result<(proto::App, Vec<proto::ThreadInfo>)> {
        match self.request(Request::Threads).await? {
            Response::Threads { app, threads } => Ok((app, threads)),
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => {
                bail!("the sysmodule predates thread backtraces (protocol 20): update it")
            }
            other => Err(unexpected(other)),
        }
    }

    /// Drops the connection, so the next request connects (and says Hello) again: to whichever daemon
    /// serves the port by then.
    pub async fn disconnect(&self) {
        *self.conn.lock().await = None;
    }

    /// Hands the console over from the NRO daemon to the sysmodule (replacing any running one) and waits
    /// until the sysmodule answers. Returns its status. A no-op if the sysmodule is already answering.
    pub async fn handover(&self) -> Result<SysmoduleStatus> {
        if self.hello().await?.kind == DaemonKind::Nro {
            let resp = self.request(Request::Handover).await;
            self.disconnect().await;
            expect_ok(resp?)?;
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
        self.disconnect().await;
        expect_ok(resp?)
    }

    /// Makes the console forget this PC's token ([`Request::Unpair`]): it can't connect again until it pairs
    /// again with the code on the console's screen.
    pub async fn unpair(&self) -> Result<()> {
        let resp = self.request(Request::Unpair).await;
        self.disconnect().await;
        match resp? {
            Response::Ok => Ok(()),
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => Err(anyhow!(
                "this Switch's Kickstand is too old to unpair (it needs protocol 25): update it, or take this PC's line out of {} on its SD card",
                proto::path::TOKEN_PATH
            )),
            other => Err(unexpected(other)),
        }
    }

    /// Samples the running application's (or applet's) call stacks every `interval_ms` for `duration_ms`.
    pub async fn profile(&self, duration_ms: u32, interval_ms: u32, thread_id: Option<u64>) -> Result<Option<crate::profile::RawProfile>> {
        match self.request(Request::Profile { duration_ms, interval_ms, thread_id }).await? {
            Response::Profile { app, elapsed_ms, ticks, samples } => {
                let samples = samples
                    .into_iter()
                    .map(|s| crate::profile::Sample { thread_id: s.thread_id, pc: s.pc, lr: s.lr, frames: s.frames })
                    .collect();
                Ok(Some(crate::profile::RawProfile { app, elapsed_ms, ticks, samples }))
            }
            Response::Error(e) if e.kind == proto::ErrorKind::Unsupported => Ok(None),
            other => Err(unexpected(other)),
        }
    }

    /// What the console's background audio capture is doing (protocol 30).
    pub async fn audio_capture(&self) -> Result<proto::AudioCapture> {
        match self.request(Request::AudioCapture).await? {
            Response::AudioCapture(c) => Ok(c),
            other => Err(too_old_for(other, "audio capture through grc:d", 30)),
        }
    }

    /// The last `seconds` (up to [`proto::audio::REPLAY_MAX_MS`]) of the audio the console has been
    /// recording in the background, as interleaved stereo samples, with the capture's state. Errors with
    /// the reason (and what to do) when there's none.
    pub async fn audio_replay(&self, seconds: f64) -> Result<(Vec<i16>, proto::AudioCapture)> {
        let capture = self.audio_capture().await?;
        let max = f64::from(proto::audio::REPLAY_MAX_MS) / 1000.0;
        let duration_ms = (seconds.clamp(0.1, max) * 1000.0).round() as u32;
        let (raw, _) = self
            .read_chunks(
                Request::AudioReplay { duration_ms },
                |offset, len| Request::AudioRead { offset, len: len as u32 },
                0,
                u64::MAX,
                "the audio replay",
            )
            .await?;
        if raw.is_empty() {
            bail!("{}", crate::audio::why_silent(&capture).unwrap_or_else(|| "the console has recorded no audio yet".into()));
        }
        Ok((crate::audio::pcm_bytes_to_i16(&raw), capture))
    }

    /// Records the next `seconds` (up to 30) of live audio as it arrives, streamed from the console.
    /// Stops early if capturing stops; errors with the reason if nothing arrived.
    /// Pauses the sysmodule's gameplay capture for `ms` (0 resumes it) so that stopping it leaves no grc:d
    /// read under way; `None` from a daemon without it (before protocol 30, or the NRO).
    pub async fn hold_capture(&self, ms: u32) -> Result<Option<proto::AudioCapture>> {
        match self.request(Request::CaptureHold { ms }).await? {
            Response::AudioCapture(c) => Ok(Some(c)),
            _ => Ok(None),
        }
    }

    pub async fn record_audio(&self, seconds: f64) -> Result<crate::audio::Recording> {
        let first = self.audio_capture().await?;
        if let Some(why) = crate::audio::why_silent(&first) {
            bail!("{why}");
        }
        let want = proto::audio::bytes_for_ms((seconds.clamp(0.1, 30.0) * 1000.0).round() as u32);
        let mut pcm = Vec::with_capacity(want as usize);
        let mut from = None;
        let mut gaps_ms = 0u64;
        let mut stopped = false;
        while (pcm.len() as u64) < want {
            let left_ms = proto::audio::ms_for_bytes(want - pcm.len() as u64) as u32;
            let req = Request::AudioStream { from, min_ms: left_ms.clamp(20, 300), max_wait_ms: 1_000 };
            let chunk = match self.request(req).await? {
                Response::AudioChunk(c) => c,
                other => return Err(too_old_for(other, "record_audio through grc:d", 30)),
            };
            if from.is_some() && chunk.skipped > 0 {
                // A gap: keep the timeline, fill it with silence.
                let fill = chunk.skipped.min(want - pcm.len() as u64) as usize & !3;
                pcm.resize(pcm.len() + fill, 0);
                gaps_ms += proto::audio::ms_for_bytes(chunk.skipped);
            }
            let take = chunk.data.len().min((want - pcm.len() as u64) as usize);
            pcm.extend_from_slice(&chunk.data[..take]);
            from = Some(chunk.from + chunk.data.len() as u64);
            if chunk.data.is_empty() && chunk.state != proto::CaptureState::Capturing {
                stopped = true;
                break;
            }
        }
        if pcm.is_empty() {
            let capture = self.audio_capture().await?;
            bail!("{}", crate::audio::why_silent(&capture).unwrap_or_else(|| "no audio arrived from the console".into()));
        }
        let stopped = if stopped { crate::audio::why_silent(&self.audio_capture().await?) } else { None };
        Ok(crate::audio::Recording { samples: crate::audio::pcm_bytes_to_i16(&pcm), gaps_ms, stopped })
    }

    /// Streams the running application's gameplay video (H.264 from the console's recorder) for about
    /// `seconds`, starting at a keyframe. The clip is empty (with the reason in `stopped`) when the
    /// console can't, or no keyframe came within a few seconds.
    pub async fn record_video(&self, seconds: f64) -> Result<crate::video::Clip> {
        let mut clip = crate::video::Clip::default();
        let mut frame = Vec::new();
        let mut restart = true;
        let started = Instant::now();
        let mut first: Option<(u64, Instant)> = None;
        let span = std::time::Duration::from_secs_f64(seconds.clamp(0.5, 30.0));
        loop {
            let chunk = match self.request(Request::VideoStream { restart }).await? {
                Response::VideoChunk(c) => c,
                other => return Err(too_old_for(other, "video capture through grc:d", 30)),
            };
            restart = false;
            clip.dropped += chunk.dropped;
            if chunk.data.is_empty() {
                if chunk.state != proto::CaptureState::Capturing {
                    clip.stopped = Some(chunk.state);
                    break;
                }
            } else {
                if chunk.frame_start {
                    frame.clear();
                }
                frame.extend_from_slice(&chunk.data);
                if chunk.frame_end {
                    let (ts0, at0) = *first.get_or_insert((chunk.timestamp, Instant::now()));
                    clip.frames.push(crate::video::Frame { data: std::mem::take(&mut frame), timestamp: chunk.timestamp.saturating_sub(ts0) });
                    clip.elapsed = at0.elapsed();
                    // The sound too, from the first frame on, picked up every few frames between them.
                    if clip.frames.len() % 8 == 1 {
                        self.video_sound(&mut clip).await;
                    }
                }
            }
            match first {
                Some((_, at)) if at.elapsed() >= span => break,
                None if started.elapsed() > crate::video::KEYFRAME_WAIT => {
                    clip.no_keyframe = true;
                    break;
                }
                _ => {}
            }
        }
        if !clip.frames.is_empty() {
            self.video_sound(&mut clip).await;
        }
        Ok(clip)
    }

    /// Adds the sound captured since the last call to `clip` (the first call starts it at the stream's
    /// head); gives up on sound for the clip if the console isn't capturing it.
    async fn video_sound(&self, clip: &mut crate::video::Clip) {
        if clip.audio_from == Some(u64::MAX) {
            return;
        }
        let req = Request::AudioStream { from: clip.audio_from, min_ms: 0, max_wait_ms: 0 };
        match self.request(req).await {
            Ok(Response::AudioChunk(c)) if c.state == proto::CaptureState::Capturing => {
                if clip.audio_from.is_some() {
                    clip.audio.resize(clip.audio.len() + c.skipped as usize, 0);
                }
                clip.audio.extend_from_slice(&c.data);
                clip.audio_from = Some(c.from + c.data.len() as u64);
            }
            _ => clip.audio_from = Some(u64::MAX),
        }
    }

    /// Queries or modifies system audio output volume (`0..=100`) and mute state.
    pub async fn audio_control(&self, volume: Option<u8>, muted: Option<bool>) -> Result<proto::AudioOutStatus> {
        match self.request(Request::AudioControl { volume, muted }).await? {
            Response::AudioStatus(status) => Ok(status),
            other => return Err(too_old_for(other, "audio_control", 29)),
        }
    }

    /// Sends one request, reconnecting and retrying once if the connection had gone stale.
    async fn request(&self, req: Request) -> Result<Response> {
        let mut guard = self.conn.lock().await;
        self.request_locked(&mut guard, req).await
    }

    /// [`Client::request`] with the connection already locked, for multi-request operations.
    async fn request_locked(&self, guard: &mut MutexGuard<'_, Option<Conn>>, req: Request) -> Result<Response> {
        if changes_the_screen(&req) {
            SCREEN_CHANGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let had_conn = guard.is_some();
        let conn = self.connected(guard).await?;
        match round_trip(conn, &req).await {
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
        // Left out of step by a cancelled call: closing it also ends the console's side (an upload's
        // staging file goes with it).
        if slot.as_ref().is_some_and(|c| c.pending > 0) {
            *slot = None;
        }
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
        let mut conn = self.connect_with(Request::Hello { version: PROTOCOL_VERSION, token: self.token.clone() }).await?;
        if let Some(app) = self.app() {
            round_trip(&mut conn, &Request::Introduce { app: introduction(&app) }).await.ok();
        }
        Ok(conn)
    }

    /// Pairs this PC with the console: `code` is the pairing code on its screen, and this client's token
    /// becomes one the console accepts. The connection stays open, authenticated.
    pub async fn pair(&self, code: &str) -> Result<DaemonInfo> {
        let mut guard = self.conn.lock().await;
        let pair = Request::Pair { version: PROTOCOL_VERSION, code: code.trim().replace(' ', ""), token: self.token.clone() };
        let conn = self.connect_with(pair).await.map_err(|e| {
            let text = format!("{e:#}");
            if text.contains("Unsupported") {
                anyhow!(
                    "the console isn't pairing: open Kickstand from hbmenu on the Switch (its screen shows the code), and update Kickstand there if it's older than this ({text})"
                )
            } else if text.contains("Unauthorized") {
                anyhow!(
                    "wrong pairing code: check the one on the Switch's screen (after {} wrong codes it shows a new one)",
                    proto::PAIR_MAX_TRIES
                )
            } else {
                e
            }
        })?;
        let info = conn.info.clone();
        *guard = Some(conn);
        Ok(info)
    }

    /// Connects and sends `first` (Hello or Pair), which the daemon answers with its Hello.
    async fn connect_with(&self, first: Request) -> Result<Conn> {
        if let Some(serial) = &self.usb {
            return connect_usb(serial, first).await;
        }
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.addr))
            .await
            .map_err(|_| anyhow!("timed out connecting to the daemon at {}: {ASLEEP}", self.addr))?
            .map_err(|e| {
                let hint = match e.kind() {
                    std::io::ErrorKind::ConnectionRefused => {
                        "something is at that address but Kickstand isn't listening: the sysmodule isn't running (the user can start it by opening Kickstand in hbmenu), or the address is another device's"
                    }
                    std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable | std::io::ErrorKind::TimedOut => ASLEEP,
                    _ => "",
                };
                let e = anyhow::Error::new(e).context(format!("connecting to the daemon at {}", self.addr));
                if hint.is_empty() { e } else { e.context(hint) }
            })?;
        stream.set_nodelay(true)?;
        let mut conn = Conn { stream: Stream::Tcp(stream), info: DaemonInfo { version: String::new(), protocol: 0, kind: DaemonKind::Nro }, pending: 0 };
        let hello = round_trip(&mut conn, &first).await?;
        greeted(conn, hello)
    }
}

/// `conn` once the daemon answered its first request with `hello`.
fn greeted(mut conn: Conn, hello: Response) -> Result<Conn> {
    match hello {
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

/// Opens the console's USB interface and sends `first` (Hello or Pair). Unlike a TCP connection, the link
/// may carry replies to an earlier connection's requests (one cancelled part way), so a Ping with a fresh
/// nonce follows `first`, and replies are read and dropped until its Pong: the one before is `first`'s.
async fn connect_usb(serial: &str, first: Request) -> Result<Conn> {
    let link = crate::usb::UsbLink::open(serial).await?;
    let mut conn = Conn { stream: Stream::Usb(Box::new(link)), info: DaemonInfo { version: String::new(), protocol: 0, kind: DaemonKind::Sysmodule }, pending: 0 };
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("getrandom: {e}"))?;
    let nonce = u64::from_le_bytes(nonce);
    send(&mut conn, &first).await?;
    send(&mut conn, &Request::Ping { nonce }).await?;
    let mut previous = None;
    loop {
        match recv_within(&mut conn, CONNECT_TIMEOUT).await.context("starting the USB connection")? {
            Response::Pong { nonce: n, .. } if n == nonce => break,
            // The Ping's answer when `first` was refused.
            Response::Error(e) if e.kind == proto::ErrorKind::Unauthorized && previous.is_some() => break,
            other => previous = Some(other),
        }
    }
    conn.pending = 0;
    let hello = previous.ok_or_else(|| anyhow!("the Switch didn't answer over USB"))?;
    greeted(conn, hello)
}

async fn round_trip(conn: &mut Conn, req: &Request) -> Result<Response> {
    send(conn, req).await?;
    recv(conn).await
}

async fn send(conn: &mut Conn, req: &Request) -> Result<()> {
    let frame = proto::encode(req)?;
    conn.pending += 1;
    let io = async {
        match &mut conn.stream {
            Stream::Tcp(stream) => stream.write_all(&frame).await.map_err(anyhow::Error::from),
            Stream::Usb(link) => link.send(&frame).await,
        }
    };
    timeout(REPLY_TIMEOUT, io).await.map_err(|_| anyhow!("daemon stopped reading for {REPLY_TIMEOUT:?}"))??;
    Ok(())
}

/// Reads the next response. Responses arrive in request order.
async fn recv(conn: &mut Conn) -> Result<Response> {
    recv_within(conn, REPLY_TIMEOUT).await
}

async fn recv_within(conn: &mut Conn, wait: Duration) -> Result<Response> {
    let io = async {
        let body = match &mut conn.stream {
            Stream::Tcp(stream) => {
                let mut prefix = [0u8; 4];
                stream.read_exact(&mut prefix).await?;
                let len = proto::frame_len(prefix).ok_or_else(|| anyhow!("daemon sent an oversized frame"))?;
                let mut body = vec![0u8; len];
                stream.read_exact(&mut body).await?;
                body
            }
            Stream::Usb(link) => link.recv().await?,
        };
        conn.pending = conn.pending.saturating_sub(1);
        let reply = proto::decode::<Response>(&body);
        // Only a reply appended after this client's version would fail here.
        anyhow::Ok(reply.with_context(|| format!("couldn't read the daemon's reply (daemon protocol {}, this client {PROTOCOL_VERSION}): update the MCP server", conn.info.protocol))?)
    };
    timeout(wait, io).await.map_err(|_| anyhow!("daemon did not reply within {wait:?}"))?
}

fn is_reset(e: &anyhow::Error) -> bool {
    e.chain().filter_map(|c| c.downcast_ref::<std::io::Error>()).any(|io| {
        matches!(io.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe)
    })
}

fn expect(resp: Response, ok: impl Fn(&Response) -> bool) -> Result<Response> {
    if ok(&resp) { Ok(resp) } else { Err(unexpected(resp)) }
}

/// Succeeds if `resp` is a plain [`Response::Ok`].
fn expect_ok(resp: Response) -> Result<()> {
    expect(resp, |r| matches!(r, Response::Ok)).map(|_| ())
}

/// [`unexpected`], but a request the daemon doesn't know (from before protocol 26) says to update it.
fn too_old(resp: Response, what: &str) -> anyhow::Error {
    too_old_for(resp, what, 26)
}

fn too_old_for(resp: Response, what: &str, protocol: u32) -> anyhow::Error {
    match resp {
        Response::Error(e) if e.kind == proto::ErrorKind::Unsupported && e.message.contains("doesn't know that request") => {
            anyhow!("Kickstand on the console predates {what} (protocol {protocol}): update it (`kickstand update --switch`, or `kickstand sysmodule install`)")
        }
        other => unexpected(other),
    }
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
                    Some((k, _)) => bail!("step {step:?}: unknown {k:?}= (l=, r= or touch=)"),
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
            bail!("stick axis {a:?} is outside -1..1");
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
        bail!("touch point {v:?} is off the 1280x720 screen");
    }
    Ok((x, y))
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
        bail!("no buttons given");
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

/// How long after its last request another agent still counts as at work.
const BUSY_MS: u64 = 120_000;

/// The other agents at work in the last two minutes, a line each ("an agent on this PC was pressing
/// buttons 12 s ago"): by the last thing each changed on the console, or with `changing` false, failing
/// that by what it last looked at.
pub fn other_agents(agents: &[proto::AgentInfo], changing: bool) -> Vec<String> {
    let mine = agents.iter().find(|a| a.you).map(|a| a.ip);
    let when = |ms: u64| if ms < 2_000 { "just now".to_string() } else { format!("{} s ago", ms / 1000) };
    agents
        .iter()
        .filter(|a| !a.you)
        .filter_map(|a| {
            let name = a.app.as_deref().map(|text| split_introduction(text).0);
            // A USB client has no address (0.0.0.0).
            let place = match a.ip {
                [0, 0, 0, 0] => "over USB".to_string(),
                ip if Some(ip) == mine => "on this PC".to_string(),
                ip => format!("at {}", std::net::Ipv4Addr::from(ip)),
            };
            let who = match name {
                Some(TERMINAL) => format!("someone using kickstand in a terminal {place}"),
                Some(DESKTOP) => format!("someone in Kickstand's desktop app {place}"),
                Some(name) => format!("an agent in {} {place}", crate::link::App::identify(name, |_| false).map_or(name, |app| app.name())),
                None => format!("an agent {place}"),
            };
            match &a.change {
                Some((what, ms)) if *ms < BUSY_MS => {
                    let now = if a.idle_ms < BUSY_MS && a.doing != *what && !proto::ROUTINE.contains(&a.doing.as_str()) && !changing {
                        format!(", then {}", a.doing)
                    } else {
                        String::new()
                    };
                    Some(format!("{who} was {what} {}{now}", when(*ms)))
                }
                _ if !changing && a.idle_ms < BUSY_MS && !proto::ROUTINE.contains(&a.doing.as_str()) => Some(format!("{who} was {} {}", a.doing, when(a.idle_ms))),
                _ => None,
            }
        })
        .collect()
}

pub fn describe_app(app: &proto::App) -> String {
    let what = match app.hbl {
        Some(true) => "hbloader (hbmenu or an NRO) in place of",
        Some(false) => "title",
        None => "title (unknown whether hbloader is in its place)",
    };
    format!("{what} {:016x}, pid {}", app.program_id, app.pid)
}

/// How many requests that can change what's in front (input, closing software, a reboot) this process has
/// sent, so [`crate::launch`] can tell whether the home menu it left showing still is.
pub(crate) static SCREEN_CHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn changes_the_screen(req: &Request) -> bool {
    matches!(
        req,
        Request::Press { .. }
            | Request::Input { .. }
            | Request::Hold { .. }
            | Request::Play { .. }
            | Request::Playback { .. }
            | Request::TypeText { .. }
            | Request::CloseApp
            | Request::CloseApplet
            | Request::RebootToHekate { .. }
    )
}

#[cfg(test)]
mod tests {
    use proto::AgentInfo;

    #[test]
    fn other_agents_at_work() {
        let agent = |ip: [u8; 4], you, doing: &str, idle_ms, change: Option<(&str, u64)>| AgentInfo {
            id: 1,
            ip,
            you,
            app: None,
            connected_ms: 600_000,
            requests: 5,
            doing: doing.into(),
            idle_ms,
            change: change.map(|(what, ms)| (what.into(), ms)),
        };
        let mut agents = [
            agent([192, 168, 1, 2], true, "pressing buttons", 0, Some(("pressing buttons", 0))),
            // Pressed, then looked at the screen.
            agent([192, 168, 1, 2], false, "looking at the screen", 800, Some(("pressing buttons", 12_400))),
            agent([192, 168, 1, 9], false, "looking at the screen", 500, None),
            agent([192, 168, 1, 2], false, "checking on the console", 100, None),
            agent([192, 168, 1, 7], false, "closing software", 300_000, Some(("closing software", 300_000))),
        ];
        assert_eq!(
            super::other_agents(&agents, false),
            [
                "an agent on this PC was pressing buttons 12 s ago, then looking at the screen",
                "an agent at 192.168.1.9 was looking at the screen just now"
            ]
        );
        assert_eq!(super::other_agents(&agents, true), ["an agent on this PC was pressing buttons 12 s ago"]);
        agents[1].app = Some("antigravity-client".into());
        assert_eq!(super::other_agents(&agents, true), ["an agent in Antigravity CLI on this PC was pressing buttons 12 s ago"]);
    }
}

