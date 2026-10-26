// SPDX-License-Identifier: GPL-3.0-only
//! Wire protocol between the kickstand daemon (on the console) and the MCP server (on the PC).
//!
//! Framing: every message is a little-endian `u32` length followed by that many bytes of postcard.
//! The client sends one [`Request`] and waits for one [`Response`]; the first request on a connection
//! must be [`Request::Hello`] with the shared token.

#![no_std]

extern crate alloc;

pub mod crc32;
pub mod path;

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to [`Request`] or [`Response`].
pub const PROTOCOL_VERSION: u32 = 1;

/// TCP port the daemon listens on.
pub const DEFAULT_PORT: u16 = 28790;

/// Largest data payload in one [`Request::UploadChunk`] or [`Response::Data`]. Kept small for the
/// sysmodule's heap.
pub const MAX_CHUNK: usize = 64 * 1024;

/// Largest encoded frame either side accepts (a chunk plus a path and some slack).
pub const MAX_FRAME: usize = MAX_CHUNK + 4096;

/// Most entries one [`Response::Dir`] carries; longer listings come back truncated.
pub const MAX_DIR_ENTRIES: usize = 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Hello { version: u32, token: String },
    Ping { nonce: u64 },
    ListDir { path: String },
    Stat { path: String },
    /// Reads up to `len` bytes (capped at [`MAX_CHUNK`]) starting at `offset`.
    ReadFile { path: String, offset: u64, len: u32 },
    /// Starts an upload of `size` bytes to `path`. Data goes to a staging file and only replaces
    /// `path` once [`Request::UploadFinish`] verifies it.
    UploadBegin { path: String, size: u64 },
    /// Chunks must arrive in order: `offset` equals the bytes received so far.
    UploadChunk { offset: u64, data: Vec<u8> },
    UploadFinish { crc32: u32 },
    UploadAbort,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Success with nothing to report (e.g. after [`Request::UploadAbort`]).
    Ok,
    Hello { version: u32, daemon_version: String, kind: DaemonKind },
    Pong { nonce: u64, uptime_ms: u64 },
    Dir { entries: Vec<DirEntry>, truncated: bool },
    Stat(DirEntry),
    Data { data: Vec<u8>, file_size: u64 },
    UploadReady { staging_path: String },
    ChunkOk { received: u64 },
    UploadDone { path: String, size: u64 },
    Error(Error),
}

/// Whether the daemon is running as the real sysmodule or as a test NRO from hbmenu.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonKind {
    Sysmodule,
    Nro,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    /// Horizon result code, when the failure came from a system call.
    pub result: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    BadRequest,
    Unauthorized,
    VersionMismatch,
    /// The path is outside what the daemon may read or write. See [`path`].
    Forbidden,
    NotFound,
    Io,
    /// An upload request arrived with no upload in progress, or out of order.
    UploadState,
    ChecksumMismatch,
}

impl Response {
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> Self {
        Response::Error(Error { kind, message: message.into(), result: None })
    }
}

/// Encodes `msg` as a complete frame (length prefix included).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, postcard::Error> {
    let mut out = postcard::to_extend(msg, Vec::from([0u8; 4]))?;
    let len = (out.len() - 4) as u32;
    out[..4].copy_from_slice(&len.to_le_bytes());
    Ok(out)
}

/// Decodes a frame body (without the length prefix).
pub fn decode<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(body)
}

/// Reads the length prefix, rejecting frames over [`MAX_FRAME`].
pub fn frame_len(prefix: [u8; 4]) -> Option<usize> {
    let len = u32::from_le_bytes(prefix) as usize;
    (len <= MAX_FRAME).then_some(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn round_trip() {
        let req = Request::UploadChunk { offset: 7, data: vec![1, 2, 3] };
        let frame = encode(&req).unwrap();
        let len = frame_len(frame[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - 4);
        assert_eq!(decode::<Request>(&frame[4..]).unwrap(), req);
    }

    #[test]
    fn max_chunk_fits_in_frame() {
        let req = Request::UploadChunk { offset: u64::MAX, data: vec![0xAA; MAX_CHUNK] };
        assert!(encode(&req).unwrap().len() - 4 <= MAX_FRAME);
        let resp = Response::Data { data: vec![0xAA; MAX_CHUNK], file_size: u64::MAX };
        assert!(encode(&resp).unwrap().len() - 4 <= MAX_FRAME);
    }

    #[test]
    fn oversized_frame_rejected() {
        assert_eq!(frame_len(((MAX_FRAME + 1) as u32).to_le_bytes()), None);
    }
}
