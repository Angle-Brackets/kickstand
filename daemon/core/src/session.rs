// SPDX-License-Identifier: GPL-3.0-only
//! One client connection's state and request handling.

use alloc::format;
use alloc::string::String;

use nx::fs::FileAccessor;
use proto::path::{self, PathError};
use proto::{DaemonKind, ErrorKind, PROTOCOL_VERSION, Request, Response, crc32};

use crate::sd;

/// Uploads are staged here, then renamed over the target once verified.
const STAGING_FILE: &str = "/kickstand/staging/upload.part";

struct Upload {
    path: String,
    file: FileAccessor,
    size: u64,
    received: u64,
    crc: u32,
}

pub struct Session<'a> {
    token: Option<&'a str>,
    kind: DaemonKind,
    authed: bool,
    upload: Option<Upload>,
}

impl<'a> Session<'a> {
    /// `token` is the shared secret clients must present; with `None` every Hello is refused.
    pub fn new(token: Option<&'a str>, kind: DaemonKind) -> Self {
        Session { token, kind, authed: false, upload: None }
    }

    pub fn handle(&mut self, req: Request) -> Response {
        match req {
            Request::Hello { version, token } => self.hello(version, &token),
            _ if !self.authed => Response::error(ErrorKind::Unauthorized, "send Hello with the token first"),
            Request::Ping { nonce } => Response::Pong { nonce, uptime_ms: crate::uptime_ms() },
            Request::ListDir { path } => self.list_dir(&path),
            Request::Stat { path } => match path::check_read(&path) {
                Ok(path) => sd::stat(&path).map_or_else(Response::Error, Response::Stat),
                Err(e) => forbidden(e),
            },
            Request::ReadFile { path, offset, len } => self.read_file(&path, offset, len),
            Request::UploadBegin { path, size } => self.upload_begin(&path, size),
            Request::UploadChunk { offset, data } => self.upload_chunk(offset, &data),
            Request::UploadFinish { crc32 } => self.upload_finish(crc32),
            Request::UploadAbort => {
                self.abort_upload();
                Response::Ok
            }
        }
    }

    fn hello(&mut self, version: u32, token: &str) -> Response {
        if version != PROTOCOL_VERSION {
            let msg = format!("daemon speaks protocol {PROTOCOL_VERSION}, client sent {version}");
            return Response::error(ErrorKind::VersionMismatch, msg);
        }
        let Some(expected) = self.token else {
            let msg = format!("no token configured: put one in {}", path::TOKEN_PATH);
            return Response::error(ErrorKind::Unauthorized, msg);
        };
        if !constant_time_eq(expected.as_bytes(), token.as_bytes()) {
            return Response::error(ErrorKind::Unauthorized, "wrong token");
        }
        self.authed = true;
        Response::Hello { version: PROTOCOL_VERSION, daemon_version: String::from(crate::VERSION), kind: self.kind }
    }

    fn list_dir(&mut self, path: &str) -> Response {
        let path = match path::check_read(path) {
            Ok(p) => p,
            Err(e) => return forbidden(e),
        };
        match sd::list_dir(&path, proto::MAX_DIR_ENTRIES) {
            Ok((entries, truncated)) => Response::Dir { entries, truncated },
            Err(e) => Response::Error(e),
        }
    }

    fn read_file(&mut self, path: &str, offset: u64, len: u32) -> Response {
        let path = match path::check_read(path) {
            Ok(p) => p,
            Err(e) => return forbidden(e),
        };
        match sd::read(&path, offset, (len as usize).min(proto::MAX_CHUNK)) {
            Ok((data, file_size)) => Response::Data { data, file_size },
            Err(e) => Response::Error(e),
        }
    }

    fn upload_begin(&mut self, path: &str, size: u64) -> Response {
        self.abort_upload();
        let path = match path::check_write(path) {
            Ok(p) => p,
            Err(e) => return forbidden(e),
        };
        let file = sd::create_dirs(path::STAGING_DIR)
            .and_then(|()| sd::create_dirs(path::parent(&path)))
            .and_then(|()| sd::create_for_write(STAGING_FILE, size));
        match file {
            Ok(file) => {
                crate::log!("upload {path}: {size} bytes");
                self.upload = Some(Upload { path, file, size, received: 0, crc: crc32::INIT });
                Response::UploadReady { staging_path: String::from(STAGING_FILE) }
            }
            Err(e) => Response::Error(e),
        }
    }

    fn upload_chunk(&mut self, offset: u64, data: &[u8]) -> Response {
        let Some(up) = self.upload.as_mut() else {
            return Response::error(ErrorKind::UploadState, "no upload in progress");
        };
        if offset != up.received || up.received + data.len() as u64 > up.size {
            let msg = format!("chunk at {offset}+{} but have {} of {}", data.len(), up.received, up.size);
            self.abort_upload();
            return Response::error(ErrorKind::UploadState, msg);
        }
        if let Err(rc) = up.file.write_array::<u8, false>(data) {
            let err = sd::io_error("write", STAGING_FILE, rc);
            self.abort_upload();
            return Response::Error(err);
        }
        up.received += data.len() as u64;
        up.crc = crc32::update(up.crc, data);
        Response::ChunkOk { received: up.received }
    }

    fn upload_finish(&mut self, expected_crc: u32) -> Response {
        let Some(up) = self.upload.take() else {
            return Response::error(ErrorKind::UploadState, "no upload in progress");
        };
        if up.received != up.size {
            let msg = format!("finished after {} of {} bytes", up.received, up.size);
            drop(up);
            let _ = sd::remove_if_exists(STAGING_FILE);
            return Response::error(ErrorKind::UploadState, msg);
        }
        let crc = crc32::finish(up.crc);
        if crc != expected_crc {
            drop(up);
            let _ = sd::remove_if_exists(STAGING_FILE);
            let msg = format!("crc32 {crc:08x}, client expected {expected_crc:08x}");
            return Response::error(ErrorKind::ChecksumMismatch, msg);
        }
        let Upload { path, file, size, .. } = up;
        if let Err(rc) = file.flush() {
            return Response::Error(sd::io_error("flush", STAGING_FILE, rc));
        }
        drop(file);
        let result = sd::remove_if_exists(&path).and_then(|()| sd::rename(STAGING_FILE, &path));
        match result {
            Ok(()) => {
                crate::log!("upload {path}: done");
                Response::UploadDone { path, size }
            }
            Err(e) => Response::Error(e),
        }
    }

    fn abort_upload(&mut self) {
        if self.upload.take().is_some() {
            let _ = sd::remove_if_exists(STAGING_FILE);
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.abort_upload();
    }
}

fn forbidden(e: PathError) -> Response {
    let kind = match e {
        PathError::ReadDenied | PathError::WriteDenied => ErrorKind::Forbidden,
        _ => ErrorKind::BadRequest,
    };
    Response::error(kind, e.describe())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
