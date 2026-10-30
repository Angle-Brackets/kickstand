// SPDX-License-Identifier: GPL-3.0-only
//! Wire protocol between the kickstand daemon (on the console) and the MCP server (on the PC).
//!
//! Framing: every message is a little-endian `u32` length followed by that many bytes of postcard.
//! Each [`Request`] gets exactly one [`Response`], in request order, so a client may send several
//! requests before reading their responses (uploads and reads pipeline chunks this way). The first
//! request on a connection must be [`Request::Hello`] with the shared token.
//!
//! # Compatibility
//! postcard writes an enum variant as its position and a struct's fields in order, without names. So a
//! message keeps its encoding only while nothing before or inside it changes, and the rules are:
//! - Never reorder, remove or change an existing variant of [`Request`] or [`Response`], or any type they
//!   carry (fields, their order and types, enum variants such as [`ErrorKind`]'s).
//! - A new feature is a new variant appended at the end of [`Request`] (and of [`Response`] if it needs a
//!   new reply), with [`PROTOCOL_VERSION`] bumped so each side can tell what the other knows. If an existing
//!   request needs more data, append a new request instead of changing it.
//! - A daemon answers a request it can't decode (one appended after its version) with
//!   [`ErrorKind::Unsupported`] and keeps the connection, so newer clients work with older daemons for
//!   everything both know. Installing a daemon always works this way, since its requests never change.
//! - [`MIN_COMPATIBLE_VERSION`] rises only if a rule had to be broken; both sides refuse a peer older than it.
//!
//! The tests pin the encoding of the install path and the order of every variant.

#![no_std]

extern crate alloc;

pub mod crc32;
pub mod hbl;
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

/// This side's protocol revision: bumped whenever a request or response is appended (see the module docs).
pub const PROTOCOL_VERSION: u32 = 20;

/// The oldest revision that still encodes every message the way this one does. Peers from this one up
/// talk to each other; older ones are refused at [`Request::Hello`].
pub const MIN_COMPATIBLE_VERSION: u32 = 17;

/// Whether a peer at protocol `version` can be talked to.
pub fn compatible(version: u32) -> bool {
    version >= MIN_COMPATIBLE_VERSION
}

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
    /// The running application's memory map. The daemon attaches as its debugger for each memory request
    /// (the application stops for that moment) and detaches before answering. Sysmodule only. Answers
    /// [`Response::Memory`].
    MemoryMap,
    /// Reads `len` bytes (capped at [`MAX_CHUNK`]) of the running application's memory at `address`.
    /// Answers [`Response::Data`] with `file_size` = the bytes read.
    ReadMemory { address: u64, len: u32 },
    /// Writes `data` (at most [`MAX_CHUNK`]) to the running application's memory at `address`. Only to
    /// homebrew (the application is hbloader), never a game's memory.
    WriteMemory { address: u64, data: Vec<u8> },
    /// (18) hbmenu's applet: the Album, which Atmosphère runs hbloader in place of (applet mode: no game
    /// needed, less memory). Answers [`Response::App`] with its process, if the Album is open.
    AppletStatus,
    /// (18) Terminated the Album's process. Since 20 refused with [`ErrorKind::Unsupported`] while the Album
    /// runs (terminating it crashes the home menu; HOME closes it); [`Response::App`] `None` if it isn't open.
    CloseApplet,
    /// (18) Which process this connection's memory requests act on: the applet (the Album) or, by default,
    /// the application. Answers [`Response::Ok`].
    MemoryTarget { applet: bool },
    /// (19) Where the console is, in one call. Answers [`Response::Status`].
    Status,
    /// (20) Arms Kickstand's entry in Atmosphère's `override_config.ini` ([`hbl`]) so launching
    /// `program_id` starts hbloader without R held (and the game itself with R held); 0 disarms it.
    /// Answers [`Response::Ok`].
    HblOverride { program_id: u64 },
    /// (20) Atmosphère fatal reports written since the daemon started (`/atmosphere/fatal_reports`): any
    /// means the console is on the fatal error screen, which only a reboot leaves. Answers
    /// [`Response::Dir`].
    FatalReports,
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
    Memory(MemoryMap),
    Error(Error),
    /// (19)
    Status(ConsoleStatus),
}

/// Answer to [`Request::Status`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ConsoleStatus {
    pub uptime_ms: u64,
    /// After an agent's reboot, the sysmodule presses B to get past the lock screen until this many ms from
    /// now (nothing says when the lock screen is gone); `None` when no presses are to come.
    pub unlocking_ms: Option<u64>,
    /// Seconds left on the keep-awake lease.
    pub keep_awake_s: u64,
    /// The application (a game, or hbloader in a game's place).
    pub application: Option<App>,
    /// The Album's process: hbloader in applet mode, or the Album itself.
    pub applet: Option<App>,
}

/// Answer to [`Request::MemoryMap`]: the mapped regions (any permission) in address order.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemoryMap {
    pub app: App,
    /// The heap: the first run of heap mappings, as (base, size).
    pub heap: Option<(u64, u64)>,
    pub regions: Vec<MemoryRegion>,
    /// There were more than [`MAX_MEMORY_REGIONS`].
    pub truncated: bool,
}

pub const MAX_MEMORY_REGIONS: usize = 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemoryRegion {
    pub base: u64,
    pub size: u64,
    /// The kernel's `MemoryState` (low byte: the type, e.g. 3 code, 4 code data, 5 heap).
    pub state: u32,
    /// `MemoryPermission` bits: 1 read, 2 write, 4 execute.
    pub perm: u32,
    /// The region starts with an NRO's header (`NRO0` at 0x10): it's the loaded homebrew's code.
    pub nro: bool,
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

    /// The install path's encoding, byte for byte: any client must be able to install any daemon.
    #[test]
    fn install_path_is_frozen() {
        use alloc::string::ToString;
        let requests: [(Request, &[u8]); 9] = [
            (Request::Hello { version: 17, token: "tok".into() }, &[0, 17, 3, b't', b'o', b'k']),
            (Request::UploadBegin { path: "/a".into(), size: 300 }, &[5, 2, b'/', b'a', 0xAC, 0x02]),
            (Request::UploadChunk { offset: 0, data: vec![9] }, &[6, 0, 1, 9]),
            (Request::UploadFinish { crc32: 0x1234_5678 }, &[7, 0xF8, 0xAC, 0xD1, 0x91, 0x01]),
            (Request::UploadAbort, &[8]),
            (Request::SysmoduleStatus, &[10]),
            (Request::SysmoduleStart, &[11]),
            (Request::Handover, &[14]),
            (Request::RebootToHekate { target: HekateTarget::Entry(1) }, &[15, 1, 1]),
        ];
        for (req, bytes) in requests {
            assert_eq!(&encode(&req).unwrap()[4..], bytes, "{req:?}");
        }
        let responses: [(Response, &[u8]); 7] = [
            (Response::Ok, &[0]),
            (Response::Hello { version: 17, daemon_version: "0.1".into(), kind: DaemonKind::Sysmodule }, &[1, 17, 3, b'0', b'.', b'1', 0]),
            (Response::UploadReady { staging_path: "/s".into() }, &[6, 2, b'/', b's']),
            (Response::ChunkOk { received: 5 }, &[7, 5]),
            (Response::UploadDone { path: "/a".into(), size: 300 }, &[8, 2, b'/', b'a', 0xAC, 0x02]),
            (Response::Sysmodule(SysmoduleStatus { installed: true, boot2: false, pid: Some(130) }), &[10, 1, 0, 1, 0x82, 0x01]),
            (Response::error(ErrorKind::Unsupported, "x".to_string()), &[16, 9, 1, b'x', 0]),
        ];
        for (resp, bytes) in responses {
            assert_eq!(&encode(&resp).unwrap()[4..], bytes, "{resp:?}");
        }
    }

    /// Every variant's position, which is how postcard encodes it. Appending a variant means adding it here
    /// (the matches won't compile otherwise); moving one fails the test.
    #[test]
    fn variants_keep_their_positions() {
        fn request(r: &Request) -> u8 {
            match r {
                Request::Hello { .. } => 0,
                Request::Ping { .. } => 1,
                Request::ListDir { .. } => 2,
                Request::Stat { .. } => 3,
                Request::ReadFile { .. } => 4,
                Request::UploadBegin { .. } => 5,
                Request::UploadChunk { .. } => 6,
                Request::UploadFinish { .. } => 7,
                Request::UploadAbort => 8,
                Request::Delete { .. } => 9,
                Request::SysmoduleStatus => 10,
                Request::SysmoduleStart => 11,
                Request::SysmoduleStop => 12,
                Request::Exit => 13,
                Request::Handover => 14,
                Request::RebootToHekate { .. } => 15,
                Request::Resources => 16,
                Request::IramPeek { .. } => 17,
                Request::Press { .. } => 18,
                Request::KeepAwake { .. } => 19,
                Request::IndicatorPosition { .. } => 20,
                Request::Screenshot => 21,
                Request::ScreenshotRead { .. } => 22,
                Request::Input { .. } => 23,
                Request::AppStatus => 24,
                Request::CloseApp => 25,
                Request::ListApps { .. } => 26,
                Request::MemoryMap => 27,
                Request::ReadMemory { .. } => 28,
                Request::WriteMemory { .. } => 29,
                Request::AppletStatus => 30,
                Request::CloseApplet => 31,
                Request::MemoryTarget { .. } => 32,
                Request::Status => 33,
                Request::HblOverride { .. } => 34,
                Request::FatalReports => 35,
            }
        }
        fn response(r: &Response) -> u8 {
            match r {
                Response::Ok => 0,
                Response::Hello { .. } => 1,
                Response::Pong { .. } => 2,
                Response::Dir { .. } => 3,
                Response::Stat(_) => 4,
                Response::Data { .. } => 5,
                Response::UploadReady { .. } => 6,
                Response::ChunkOk { .. } => 7,
                Response::UploadDone { .. } => 8,
                Response::Deleted { .. } => 9,
                Response::Sysmodule(_) => 10,
                Response::Resources(_) => 11,
                Response::KeepAwake { .. } => 12,
                Response::App(_) => 13,
                Response::Apps(_) => 14,
                Response::Memory(_) => 15,
                Response::Error(_) => 16,
                Response::Status(_) => 17,
            }
        }
        let s = || String::from("/a");
        let requests = [
            Request::Hello { version: 0, token: s() },
            Request::Ping { nonce: 0 },
            Request::ListDir { path: s() },
            Request::Stat { path: s() },
            Request::ReadFile { path: s(), offset: 0, len: 0 },
            Request::UploadBegin { path: s(), size: 0 },
            Request::UploadChunk { offset: 0, data: Vec::new() },
            Request::UploadFinish { crc32: 0 },
            Request::UploadAbort,
            Request::Delete { path: s(), recursive: false },
            Request::SysmoduleStatus,
            Request::SysmoduleStart,
            Request::SysmoduleStop,
            Request::Exit,
            Request::Handover,
            Request::RebootToHekate { target: HekateTarget::Menu },
            Request::Resources,
            Request::IramPeek { offset: 0 },
            Request::Press { buttons: Vec::new(), times: 1, hold_ms: 0 },
            Request::KeepAwake { minutes: None },
            Request::IndicatorPosition { x: 0, y: 0 },
            Request::Screenshot,
            Request::ScreenshotRead { offset: 0, len: 0 },
            Request::Input { steps: Vec::new() },
            Request::AppStatus,
            Request::CloseApp,
            Request::ListApps { max: 0 },
            Request::MemoryMap,
            Request::ReadMemory { address: 0, len: 0 },
            Request::WriteMemory { address: 0, data: Vec::new() },
            Request::AppletStatus,
            Request::CloseApplet,
            Request::MemoryTarget { applet: true },
            Request::Status,
            Request::HblOverride { program_id: 0 },
            Request::FatalReports,
        ];
        for (i, req) in requests.iter().enumerate() {
            assert_eq!((request(req) as usize, encode(req).unwrap()[4]), (i, i as u8), "{req:?}");
        }
        let app = App { pid: 0, program_id: 0, hbl: None };
        let responses = [
            Response::Ok,
            Response::Hello { version: 0, daemon_version: s(), kind: DaemonKind::Nro },
            Response::Pong { nonce: 0, uptime_ms: 0 },
            Response::Dir { entries: Vec::new(), truncated: false },
            Response::Stat(DirEntry { name: s(), is_dir: false, size: 0 }),
            Response::Data { data: Vec::new(), file_size: 0 },
            Response::UploadReady { staging_path: s() },
            Response::ChunkOk { received: 0 },
            Response::UploadDone { path: s(), size: 0 },
            Response::Deleted { path: s(), was_dir: false },
            Response::Sysmodule(SysmoduleStatus { installed: false, boot2: false, pid: None }),
            Response::Resources(Resources { limits: Vec::new(), process_used: 0, process_total: 0 }),
            Response::KeepAwake { remaining_s: 0 },
            Response::App(None),
            Response::Apps(Vec::new()),
            Response::Memory(MemoryMap { app, heap: None, regions: Vec::new(), truncated: false }),
            Response::error(ErrorKind::Io, s()),
            Response::Status(ConsoleStatus { uptime_ms: 0, unlocking_ms: None, keep_awake_s: 0, application: None, applet: None }),
        ];
        for (i, resp) in responses.iter().enumerate() {
            assert_eq!((response(resp) as usize, encode(resp).unwrap()[4]), (i, i as u8), "{resp:?}");
        }
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
