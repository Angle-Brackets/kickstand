// SPDX-License-Identifier: GPL-3.0-only
//! A stand-in daemon that serves a local directory as the "SD card". It follows the same protocol and
//! path rules as the real one, so the client and MCP tools can be tested without the console.
//! It is not a model of the console's quirks (FS result codes, memory limits, timing).

use std::path::{Path, PathBuf};

use anyhow::Result;
use proto::path;
use proto::{DaemonKind, DirEntry, Error, ErrorKind, PROTOCOL_VERSION, Request, Response, crc32};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub async fn serve(listener: TcpListener, root: PathBuf, token: String) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        // One client at a time, like the daemon.
        let _ = handle(stream, &root, &token).await;
    }
}

struct Upload {
    path: String,
    data: Vec<u8>,
    size: u64,
}

async fn handle(mut stream: TcpStream, root: &Path, token: &str) -> Result<()> {
    let mut authed = false;
    let mut upload: Option<Upload> = None;
    loop {
        let mut prefix = [0u8; 4];
        stream.read_exact(&mut prefix).await?;
        let Some(len) = proto::frame_len(prefix) else { return Ok(()) };
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await?;
        let req: Request = proto::decode(&body)?;
        let resp = match req {
            Request::Hello { version, token: t } => {
                if version != PROTOCOL_VERSION {
                    Response::error(ErrorKind::VersionMismatch, "version")
                } else if t != token {
                    Response::error(ErrorKind::Unauthorized, "wrong token")
                } else {
                    authed = true;
                    Response::Hello { version, daemon_version: "mock".into(), kind: DaemonKind::Nro }
                }
            }
            _ if !authed => Response::error(ErrorKind::Unauthorized, "send Hello first"),
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
            Request::Press { .. } => Response::Ok,
            Request::KeepAwake { on } => Response::KeepAwake { on: on.unwrap_or(false) },
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
