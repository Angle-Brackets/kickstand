// SPDX-License-Identifier: GPL-3.0-only
//! Async client for the daemon. Keeps one connection open and reconnects when it drops (the daemon
//! closes idle clients after two minutes, and Wi-Fi comes and goes).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use proto::{DaemonKind, DirEntry, PROTOCOL_VERSION, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// SD writes of a 64 KiB chunk are quick, but the first write after creating a large file can stall.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

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
        let mut out = Vec::new();
        let mut file_size;
        loop {
            let at = offset + out.len() as u64;
            let want = (len - out.len() as u64).min(proto::MAX_CHUNK as u64) as u32;
            let req = Request::ReadFile { path: path.into(), offset: at, len: want };
            match self.request(req).await? {
                Response::Data { data, file_size: size } => {
                    file_size = size;
                    let done = data.is_empty();
                    out.extend_from_slice(&data);
                    if done || out.len() as u64 >= len || at + data.len() as u64 >= size {
                        break;
                    }
                }
                other => return Err(unexpected(other)),
            }
        }
        Ok((out, file_size))
    }

    /// Uploads `data` to `path`, verified with CRC-32 before it replaces the old file.
    pub async fn upload(&self, path: &str, data: &[u8]) -> Result<UploadStats> {
        let start = Instant::now();
        let mut guard = self.conn.lock().await;
        // The whole upload runs on one connection: the daemon's staging state is per connection.
        let result = async {
            let conn = self.connected(&mut guard).await?;
            let begin = Request::UploadBegin { path: path.into(), size: data.len() as u64 };
            expect(round_trip(conn, &begin).await?, |r| matches!(r, Response::UploadReady { .. }))?;
            let mut offset = 0u64;
            for chunk in data.chunks(proto::MAX_CHUNK) {
                let req = Request::UploadChunk { offset, data: chunk.to_vec() };
                offset += chunk.len() as u64;
                expect(round_trip(conn, &req).await?, |r| matches!(r, Response::ChunkOk { received } if *received == offset))?;
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

    /// Sends one request, reconnecting and retrying once if the connection had gone stale.
    async fn request(&self, req: Request) -> Result<Response> {
        let mut guard = self.conn.lock().await;
        let had_conn = guard.is_some();
        let first = match self.connected(&mut guard).await {
            Ok(conn) => round_trip(conn, &req).await,
            Err(e) => return Err(e),
        };
        match first {
            Ok(resp) => Ok(resp),
            Err(e) if had_conn => {
                *guard = None;
                let conn = self.connected(&mut guard).await.context(format!("after: {e:#}"))?;
                let resp = round_trip(conn, &req).await;
                if resp.is_err() {
                    *guard = None;
                }
                resp
            }
            Err(e) => {
                *guard = None;
                Err(e)
            }
        }
    }

    async fn connected<'a>(&self, slot: &'a mut Option<Conn>) -> Result<&'a mut Conn> {
        if slot.is_none() {
            *slot = Some(self.connect().await?);
        }
        Ok(slot.as_mut().unwrap())
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
    let frame = proto::encode(req)?;
    let io = async {
        conn.stream.write_all(&frame).await?;
        let mut prefix = [0u8; 4];
        conn.stream.read_exact(&mut prefix).await?;
        let len = proto::frame_len(prefix).ok_or_else(|| anyhow!("daemon sent an oversized frame"))?;
        let mut body = vec![0u8; len];
        conn.stream.read_exact(&mut body).await?;
        anyhow::Ok(proto::decode::<Response>(&body)?)
    };
    timeout(REPLY_TIMEOUT, io).await.map_err(|_| anyhow!("daemon did not reply within {REPLY_TIMEOUT:?}"))?
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

/// Rejects a path the daemon would refuse, before any network round trip.
pub fn check_path(path: &str, write: bool) -> Result<String> {
    let checked = if write { proto::path::check_write(path) } else { proto::path::check_read(path) };
    checked.map_err(|e| anyhow!("{path}: {}", e.describe()))
}
