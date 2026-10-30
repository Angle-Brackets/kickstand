// SPDX-License-Identifier: GPL-3.0-only
//! Wire protocol between the kickstand daemon (on the console) and the MCP server (on the PC).
//!
//! Framing: every message is a little-endian `u32` length followed by that many bytes of postcard.
//! Each [`Request`] gets exactly one [`Response`], in request order, so a client may send several
//! requests before reading their responses (uploads and reads pipeline chunks this way). The first
//! request on a connection must be [`Request::Hello`] with the shared token.

#![no_std]

extern crate alloc;

pub mod crc32;
pub mod path;

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// hekate's payload layout (hekate `bdk/utils/types.h`, `bootloader/main.c`; facts only, hekate is
/// GPL-2.0): the boot config block, which hekate reads at start to decide what to do, and the version
/// block after it.
pub mod hekate {
    pub const BOOT_CFG_OFFSET: usize = 0x94;
    pub const BOOT_CFG_SIZE: usize = 0x84;
    pub const MAGIC_OFFSET: usize = BOOT_CFG_OFFSET + BOOT_CFG_SIZE;
    pub const MAGIC: &[u8; 4] = b"ICTC";

    const BOOT_CFG_AUTOBOOT_EN: u8 = 1 << 0;
    /// Launched by another program: skips the boot logo wait and the VOL- check.
    const BOOT_CFG_FROM_LAUNCH: u8 = 1 << 1;
    const EXTRA_CFG_NYX_UMS: u8 = 1 << 5;
    const NYX_UMS_SD_CARD: u8 = 0;

    /// The boot entries of a `hekate_ipl.ini`, in the order hekate numbers them for autoboot: the
    /// `[sections]` after `[config]`, the first being 1. Captions (`{...}`) and comments don't count.
    pub fn entries(ini: &str) -> alloc::vec::Vec<&str> {
        let mut after_config = false;
        let mut out = alloc::vec::Vec::new();
        for line in ini.lines().map(str::trim) {
            let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) else { continue };
            if !after_config {
                after_config = name == "config";
            } else {
                out.push(name);
            }
        }
        out
    }

    /// Whether `payload` looks like hekate (its magic after the boot config).
    pub fn is_hekate(payload: &[u8]) -> bool {
        payload.get(MAGIC_OFFSET..MAGIC_OFFSET + 4) == Some(&MAGIC[..])
    }

    /// The boot config block for `target`: `{ u8 boot_cfg, autoboot, autoboot_list, extra_cfg; u8 xt[0x80] }`.
    pub fn boot_cfg(target: super::HekateTarget) -> [u8; BOOT_CFG_SIZE] {
        let mut cfg = [0u8; BOOT_CFG_SIZE];
        match target {
            super::HekateTarget::Menu => {}
            super::HekateTarget::Entry(index) => {
                cfg[0] = BOOT_CFG_AUTOBOOT_EN | BOOT_CFG_FROM_LAUNCH;
                cfg[1] = index;
            }
            super::HekateTarget::Ums => {
                cfg[3] = EXTRA_CFG_NYX_UMS;
                cfg[4] = NYX_UMS_SD_CARD;
            }
        }
        cfg
    }
}

/// Bumped on any incompatible change to [`Request`] or [`Response`].
pub const PROTOCOL_VERSION: u32 = 15;

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
    /// Deletes a file, or a directory (which must be empty unless `recursive`). Only inside the write
    /// roots, like uploads ([`path::check_write`]).
    Delete { path: String, recursive: bool },
    /// Reports on the sysmodule ([`path::SYSMODULE_PROGRAM_ID`]); the next two also act on it.
    SysmoduleStatus,
    /// Starts the installed sysmodule (pm:shell), whether or not it boots with the console.
    SysmoduleStart,
    /// Terminates the sysmodule. Refused by the sysmodule itself.
    SysmoduleStop,
    /// Ends the NRO daemon after replying, returning the console to hbmenu (and freeing the port for
    /// the sysmodule). Refused by the sysmodule.
    Exit,
    /// NRO only: replies, stops any running sysmodule, closes its listener, then starts the installed
    /// sysmodule and exits, so the sysmodule's first bind finds the port free.
    Handover,
    /// Warm reboot into hekate (Atmosphère's `reboot_payload.bin`), never a cold reboot. The daemon replies
    /// first, then reboots; it doesn't come back unless the sysmodule starts at boot. The NRO goes through
    /// ams_mitm's redirected `bpc`; the sysmodule loads the payload into IRAM itself.
    RebootToHekate { target: HekateTarget },
    /// How much of its resource limit (memory, threads, events, transfer memories, sessions) the
    /// daemon's group uses, and the daemon's own memory. For the sysmodule that group is the one every
    /// system process shares; for the NRO it's the application's.
    Resources,
    /// Debug, sysmodule only: reads the 4 KiB page of IRAM at `offset` (page aligned, below 0x40000) with
    /// Atmosphère's IramCopy smc, the call the sysmodule's reboot uses to load the payload. Answers
    /// [`Response::Data`] with `file_size` = the IRAM size.
    IramPeek { offset: u32 },
    /// Presses `buttons` together, `times` times, each held `hold_ms`, on a virtual Pro Controller that is
    /// attached for the call (about a second to settle, then the presses) and detached after. The lock
    /// screen wants the same button three times.
    Press { buttons: Vec<Button>, times: u8, hold_ms: u32 },
    /// Keeps the screen from dimming and the console from auto-sleeping (reporting user activity to
    /// `idle:sys` every 20 s) for `minutes` from now, at most 8 hours. Every request already keeps it awake
    /// for 30 minutes, so this is for longer stretches without requests (watching a game run). `Some(0)` ends
    /// the lease; `None` only asks. Not saved: a reboot ends it. Answers [`Response::KeepAwake`].
    KeepAwake { minutes: Option<u32> },
    /// Debug, sysmodule: moves the "agent working" indicator's top-left corner (vi's 1920x1080 layer space) until
    /// the next boot, to line it up with the home menu's status bar.
    IndicatorPosition { x: u16, y: u16 },
    /// Captures the screen as a JPEG (what the Capture button would save: the kickstand indicator isn't in
    /// it) and keeps it for this connection. Answers [`Response::Data`] with the first chunk and
    /// `file_size` = the JPEG's size; [`Request::ScreenshotRead`] fetches the rest.
    Screenshot,
    /// Reads up to `len` bytes (capped at [`MAX_CHUNK`]) at `offset` of the last [`Request::Screenshot`].
    ScreenshotRead { offset: u64, len: u32 },
    /// Plays `steps` in order on one virtual controller (attached for the call, about a second to
    /// settle), each holding exactly its buttons for its time, then releases everything. A button in
    /// consecutive steps stays held: hold R through pressing A and the load to open hbmenu in a title.
    /// At most [`MAX_INPUT_STEPS`] steps and [`MAX_INPUT_MS`] in all.
    Input { steps: Vec<InputStep> },
    /// What application is running, if any. Answers [`Response::App`].
    AppStatus,
    /// Terminates the running application (pm:shell), whatever state it's in. Unlike closing it from the
    /// home menu, the application isn't asked to save. Answers [`Response::App`] with what was closed.
    CloseApp,
    /// The installed applications, most recently used first (the order of the home menu's row, which
    /// shows the first [`HOME_MENU_TILES`]). Answers [`Response::Apps`].
    ListApps { max: u16 },
}

/// One step of [`Request::Input`]: hold exactly `buttons` (none: a pause), the sticks and a touch for `ms`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct InputStep {
    pub buttons: Vec<Button>,
    pub ms: u32,
    /// Left stick (x right, y up), each -[`STICK_MAX`]..=[`STICK_MAX`]; `None` is centred.
    pub left: Option<(i32, i32)>,
    pub right: Option<(i32, i32)>,
    /// A finger on the touch screen at (x, y) in its 1280x720 space.
    pub touch: Option<(u16, u16)>,
}

/// A stick pushed all the way.
pub const STICK_MAX: i32 = 0x7FFF;

pub const MAX_INPUT_STEPS: usize = 64;
/// Keeps an [`Request::Input`] call under the client's 30 s reply timeout.
pub const MAX_INPUT_MS: u32 = 20_000;
/// Titles on the home menu's row before "All Software" (which can't open hbmenu).
pub const HOME_MENU_TILES: usize = 12;

/// A controller button, for [`Request::Press`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    A,
    B,
    X,
    Y,
    LStick,
    RStick,
    L,
    R,
    Zl,
    Zr,
    Plus,
    Minus,
    Left,
    Up,
    Right,
    Down,
    Home,
    Capture,
}

impl Button {
    pub const ALL: [Button; 18] = [
        Button::A,
        Button::B,
        Button::X,
        Button::Y,
        Button::LStick,
        Button::RStick,
        Button::L,
        Button::R,
        Button::Zl,
        Button::Zr,
        Button::Plus,
        Button::Minus,
        Button::Left,
        Button::Up,
        Button::Right,
        Button::Down,
        Button::Home,
        Button::Capture,
    ];

    /// Its bit in `HidNpadButton` (HOME and Capture as hid:dbg numbers them).
    pub fn bit(self) -> u64 {
        match self {
            Button::Home => 1 << 18,
            Button::Capture => 1 << 19,
            b => 1 << (b as u32),
        }
    }

    /// Lowercase name, as the CLI and MCP tool spell it.
    pub fn name(self) -> &'static str {
        match self {
            Button::A => "a",
            Button::B => "b",
            Button::X => "x",
            Button::Y => "y",
            Button::LStick => "lstick",
            Button::RStick => "rstick",
            Button::L => "l",
            Button::R => "r",
            Button::Zl => "zl",
            Button::Zr => "zr",
            Button::Plus => "plus",
            Button::Minus => "minus",
            Button::Left => "left",
            Button::Up => "up",
            Button::Right => "right",
            Button::Down => "down",
            Button::Home => "home",
            Button::Capture => "capture",
        }
    }

    pub fn from_name(name: &str) -> Option<Button> {
        Button::ALL.into_iter().find(|b| b.name().eq_ignore_ascii_case(name))
    }
}

/// What hekate does after a [`Request::RebootToHekate`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HekateTarget {
    /// Show hekate's menu (someone has to pick an entry on the touchscreen).
    Menu,
    /// Boot this `hekate_ipl.ini` entry, counting from 1 and skipping `[config]`.
    Entry(u8),
    /// Start USB mass storage of the SD card in Nyx, so a PC can mount it.
    Ums,
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
    Deleted { path: String, was_dir: bool },
    Sysmodule(SysmoduleStatus),
    Resources(Resources),
    /// Seconds left on the keep-awake lease, 0 if none.
    KeepAwake { remaining_s: u64 },
    App(Option<App>),
    Apps(Vec<AppEntry>),
    Error(Error),
}

/// The running application.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub pid: u64,
    /// The title it was launched as (for hbmenu, the title it took over).
    pub program_id: u64,
    /// Running as hbloader (hbmenu or an NRO) rather than the title itself; `None` if Atmosphère didn't say.
    pub hbl: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub id: u64,
    /// Empty if ns couldn't read it (e.g. a game card that isn't inserted).
    pub name: String,
}

/// Answer to [`Request::Resources`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Resources {
    /// One per limitable resource, in kernel order: memory (bytes), threads, events, transfer
    /// memories, sessions.
    pub limits: Vec<ResourceUsage>,
    /// The daemon process's own memory in use and available, in bytes.
    pub process_used: u64,
    pub process_total: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ResourceUsage {
    pub name: String,
    pub current: i64,
    pub peak: i64,
    pub limit: i64,
}

/// Whether the daemon is running as the real sysmodule or as a test NRO from hbmenu.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonKind {
    Sysmodule,
    Nro,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysmoduleStatus {
    /// `exefs.nsp` is on the SD card.
    pub installed: bool,
    /// `flags/boot2.flag` is on the SD card, so it starts at boot.
    pub boot2: bool,
    /// Its process id, if it's running.
    pub pid: Option<u64>,
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
    /// An upload request arrived with no upload in progress, or out of order, or a delete arrived
    /// during an upload.
    UploadState,
    ChecksumMismatch,
    /// A system service call failed (`result` has the code).
    System,
    /// The request doesn't apply to this kind of daemon (e.g. `Exit` sent to the sysmodule).
    Unsupported,
    /// The daemon is already serving as many clients as it can; sent once, then the connection closes.
    Busy,
    /// The console didn't have the memory for this (e.g. a screenshot while others hold theirs).
    OutOfMemory,
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
    fn hekate_boot_cfg() {
        assert_eq!(hekate::boot_cfg(HekateTarget::Menu), [0; 0x84]);
        let entry = hekate::boot_cfg(HekateTarget::Entry(2));
        assert_eq!(entry[..4], [0b11, 2, 0, 0]);
        let ums = hekate::boot_cfg(HekateTarget::Ums);
        assert_eq!(ums[..5], [0, 0, 0, 0x20, 0]);
        let mut payload = vec![0u8; 0x200];
        payload[0x118..0x11C].copy_from_slice(b"ICTC");
        assert!(hekate::is_hekate(&payload));
        assert!(!hekate::is_hekate(&payload[..0x100]));
        let ini = "[config]\nupdater2p=1\n\n[Atmosphere emuMMC]\npkg3=x\n{caption}\n#[comment]\n[Stock]\nstock=1\n";
        assert_eq!(hekate::entries(ini), ["Atmosphere emuMMC", "Stock"]);
        assert!(hekate::entries("[a]\n[b]").is_empty());
    }

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
