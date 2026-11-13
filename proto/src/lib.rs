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

mod bytes;
pub mod crc32;
pub mod discovery;
pub mod hbl;
pub mod path;
pub mod tokens;

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
pub const PROTOCOL_VERSION: u32 = 30;

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

/// The protocol over a USB cable ([`Request::UsbOpen`]). The console is a USB device with one vendor-specific
/// interface ([`usb::CLASS`]) of a bulk OUT and a bulk IN endpoint. Each frame, length prefix and all, is one
/// transfer that ends in a short packet: a frame whose length is a multiple of 64 bytes gets one more byte
/// (zero), so it does at every speed (packets are 64 bytes at full speed, 512 at high, 1024 at super), and the
/// receiver ignores a byte after the frame. Requests and replies alternate as on TCP, several requests may be
/// queued, and nothing else crosses the link.
pub mod usb {
    /// libnx's default ids for a homebrew USB device; [`PRODUCT`] and the serial number tell ours apart.
    pub const VENDOR_ID: u16 = 0x057E;
    pub const PRODUCT_ID: u16 = 0x3000;
    pub const MANUFACTURER: &str = "Kickstand";
    pub const PRODUCT: &str = "Kickstand";
    /// The interface's class, subclass and protocol: vendor-specific, then "KS".
    pub const CLASS: (u8, u8, u8) = (0xFF, 0x4B, 0x53);
    /// The interface goes away after this long without a request.
    pub const IDLE_MS: u64 = 60_000;
    /// ... or this long after [`super::Request::UsbOpen`] if the PC hasn't opened it.
    pub const ATTACH_MS: u64 = 20_000;
    /// Longest transfer: the largest frame, its prefix and the padding byte.
    pub const MAX_TRANSFER: usize = super::MAX_FRAME + 5;

    /// `frame` (an encoded message, prefix included) as one transfer.
    pub fn pad(frame: &mut alloc::vec::Vec<u8>) {
        if frame.len().is_multiple_of(64) {
            frame.push(0);
        }
    }

    /// The message in a received transfer (without its prefix), if the transfer holds exactly one frame.
    pub fn unpad(transfer: &[u8]) -> Option<&[u8]> {
        let prefix: [u8; 4] = transfer.get(..4)?.try_into().ok()?;
        let len = super::frame_len(prefix)?;
        let end = 4 + len;
        let padded = end.is_multiple_of(64);
        (transfer.len() == end || (padded && transfer.len() == end + 1)).then(|| &transfer[4..end])
    }
}

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
    UploadChunk {
        offset: u64,
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
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
    WriteMemory {
        address: u64,
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
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
    /// (20) Pauses the running application (or with [`Request::MemoryTarget`], the applet) for a moment
    /// and returns each thread's registers and frame-pointer backtrace. Answers [`Response::Threads`].
    Threads,
    /// (21) In place of [`Request::Hello`] on a PC that has no token for this console yet: `code` is the
    /// pairing code the console shows ([`PAIR_CODE_LEN`] digits), and `token` the PC's own token, which
    /// the console then accepts from now on. Only the on-console screen (the NRO) pairs, as only it shows
    /// a code. Answers [`Response::Hello`], and the connection is authenticated as after Hello.
    Pair { version: u32, code: String, token: String },
    /// (22) Who is connected, what they have been doing and the keep-awake lease, for a status display
    /// (the desktop app). Unlike every other request it isn't counted as activity and doesn't extend the
    /// lease, so a display left open neither shows up as an agent nor keeps the console awake. Answers
    /// [`Response::Activity`].
    Activity,
    /// (23) Holds `step`'s buttons, sticks and touch on a virtual controller that stays attached for
    /// `step.ms` (at most [`MAX_HOLD_LEASE_MS`]); then everything is released and the controller detached,
    /// so a client that goes away never leaves a button held. Sent again, it changes what's held and
    /// renews the lease; `ms` 0 releases and detaches now. An empty step keeps the controller attached
    /// holding nothing, so [`Request::Press`] and [`Request::Input`] meanwhile skip the second a new
    /// controller needs to settle. Answers [`Response::Ok`] once it's held.
    Hold { step: InputStep },
    /// (24) Each connected client: which AI app it works for ([`Request::Introduce`]), its last request and
    /// its last one that changed something on the console (not [`ROUTINE`] or [`LOOKING`]), so an agent
    /// can tell another is at work even while that one is looking at the screen between presses; and what
    /// clients did lately, by client. Passive like [`Request::Activity`]. Answers [`Response::Agents`].
    Agents,
    /// (24) Names the AI app this client works for: an MCP server sends the app it was linked to, or what
    /// the app called itself in MCP's handshake. Cut to [`MAX_APP_NAME`] bytes. Passive like
    /// [`Request::Activity`]. Answers [`Response::Ok`].
    Introduce { app: String },
    /// (25) Forgets the token this connection presented: it comes out of the console's token file (with
    /// its `# paired:` comment), so the PC has to pair again with the code on the console's screen. A token
    /// given on the NRO's command line can't be forgotten this way. Answers [`Response::Ok`]; the
    /// connection isn't authenticated after it.
    Unpair,
    /// (26) Plays `steps` like [`Request::Input`] (at most [`MAX_PLAY_STEPS`] and [`MAX_INPUT_MS`]), but on
    /// the held controller of [`Request::Hold`] (attaching it if need be), and then keeps holding the last
    /// step for `lease_ms` (at most [`MAX_HOLD_LEASE_MS`]) instead of letting go: a long recording is sent
    /// as several of these back to back, with no release or settling between them. Finish with a Hold of
    /// `ms` 0. Answers [`Response::Ok`] when the steps have played.
    Play { steps: Vec<InputStep>, lease_ms: u32 },
    /// (26) Starts recording the physical controllers (buttons and sticks, all players and handheld
    /// merged; not HOME or Capture) for at most `max_ms` ([`MAX_RECORD_MS`]); a recording in progress is
    /// thrown away. Answers [`Response::Ok`].
    RecordStart { max_ms: u32 },
    /// (26) Stops recording (or, after it stopped by itself, just reports). Answers
    /// [`Response::Recording`]; read its inputs with [`Request::RecordRead`].
    RecordStop,
    /// (26) The recorded inputs from number `from` on, at most [`MAX_RECORD_PAGE`]. Answers
    /// [`Response::Recorded`].
    RecordRead { from: u32 },
    /// (26) Types `text` on a virtual USB keyboard (for the software keyboard, or anything that reads a
    /// keyboard), at most [`MAX_TYPE_TEXT`] characters of printable ASCII and newlines. Answers
    /// [`Response::Ok`] once typed.
    TypeText { text: String },
    /// (26) Files and folders under `dir` (searched depth first) whose name contains `name` (ignoring
    /// case; empty matches all), at most `max` ([`MAX_FIND`]). Folders the daemon won't read are skipped.
    /// Answers [`Response::Found`].
    Find { dir: String, name: String, max: u32 },
    /// (26) Lines containing `needle` (ignoring case if asked) in the file `path`, or in every file under it
    /// if it's a folder (files over [`MAX_GREP_FILE`] skipped), at most `max` ([`MAX_GREP`]). Answers
    /// [`Response::Matches`].
    Grep { path: String, needle: String, ignore_case: bool, max: u32 },
    /// (27) Pauses, resumes or stops the [`Request::Play`]s running on any connection (a macro another
    /// client is replaying), or just asks. Pausing lets go of everything and stops the clock until resumed;
    /// a pause longer than [`MAX_PAUSE_MS`] stops the macro. A stopped Play answers [`Response::Playback`]
    /// at once instead of [`Response::Ok`], and so does every Play after it on that connection until
    /// another request. Answers [`Response::Playback`] with how things were when it arrived.
    Playback { action: PlaybackAction },
    /// (28) Starts Kickstand's USB interface on the console's USB-C port, for a cable to the PC: the PC
    /// opens the device ([`usb`]) whose serial number is the one answered and talks this protocol over it, a
    /// connection like any other that starts with Hello. The interface goes away after
    /// [`Request::UsbClose`], [`usb::IDLE_MS`] without a request, [`usb::ATTACH_MS`] without the PC opening
    /// it, or when the cable is pulled; while it's up, other USB homebrew (haze, DBI, Goldleaf) can't use
    /// the port. Sysmodule only; asked again while up, answers the same serial. Answers
    /// [`Response::UsbOpened`].
    UsbOpen,
    /// (28) Ends the USB interface (after this reply, when sent over it). Answers [`Response::Ok`].
    UsbClose,
    /// (29) Samples the running application's (or applet's) call stacks every `interval_ms` for
    /// `duration_ms` (optionally restricted to `thread_id`). Answers [`Response::Profile`].
    Profile { duration_ms: u32, interval_ms: u32, thread_id: Option<u64> },
    /// (29) Marks the last `duration_ms` (at most [`audio::REPLAY_MAX_MS`]) of the audio the sysmodule has
    /// been recording in the background ([`audio`]) for reading with [`Request::AudioRead`]: less when less
    /// has been recorded since the application started or its sound last resumed. Answers [`Response::Data`]
    /// with the first chunk and `file_size` the snapshot's length in bytes (0 when nothing is recorded; ask
    /// [`Request::AudioCapture`] why), or an error when this daemon can't capture.
    AudioReplay { duration_ms: u32 },
    /// (29) Reads `len` bytes from `offset` of this connection's last [`Request::AudioReplay`] snapshot.
    /// Answers [`Response::Data`], or an error if that part has since been overwritten (read faster).
    AudioRead { offset: u64, len: u32 },
    /// (29) Replaced by [`Request::AudioStream`] in protocol 30; answered [`ErrorKind::Unsupported`].
    AudioStreamChunk { duration_ms: u32 },
    /// (29) Queries or updates system audio output volume (`0..=100`) and mute state. Answers
    /// [`Response::AudioStatus`].
    AudioControl { volume: Option<u8>, muted: Option<bool> },
    /// (30) What the background audio capture is doing ([`audio`]). Answers [`Response::AudioCapture`].
    AudioCapture,
    /// (30) Live audio from byte `from` of the capture's stream ([`AudioCapture::cursor`]; `None` = from
    /// now), waiting up to `max_wait_ms` (at most [`audio::MAX_WAIT_MS`]) until at least `min_ms` of it has
    /// arrived, or less if capturing stops. Answers [`Response::AudioChunk`] with at most [`MAX_CHUNK`] bytes.
    AudioStream { from: Option<u64>, min_ms: u32, max_wait_ms: u32 },
    /// (30) The next piece of the running application's gameplay video (H.264), read from `grc:d` while
    /// a client keeps asking (it stops [`video::IDLE_MS`] after the last ask). `restart` drops what was
    /// queued, to start a new clip. Frames come in pieces of at most [`MAX_CHUNK`] bytes; one reply waits
    /// up to [`video::MAX_WAIT_MS`]. One client at a time. Answers [`Response::VideoChunk`].
    VideoStream { restart: bool },
    /// (30) Pause gameplay capture (audio and video) for `ms` (0 resumes it), waiting up to 1.5 s for the
    /// `grc:d` Transfers under way to finish. Send it before anything stops the sysmodule while the game is
    /// in front: grc sends nothing to a new reader after one was stopped mid-Transfer. Answers
    /// [`Response::AudioCapture`], whose state is [`CaptureState::Held`] when nothing is left under way.
    CaptureHold { ms: u32 },
}

/// What [`Request::Playback`] does.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackAction {
    Query,
    Pause,
    Resume,
    Stop,
}

/// Whether a macro is playing ([`Response::Playback`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Playback {
    pub playing: bool,
    pub paused: bool,
    /// What the playing client introduced itself as ([`Request::Introduce`]), if it did.
    pub app: Option<String>,
}

/// Longest a macro stays paused before it's stopped (the playing client waits for its reply meanwhile).
pub const MAX_PAUSE_MS: u32 = 10 * 60_000;

/// Most steps in one [`Request::Play`].
pub const MAX_PLAY_STEPS: usize = 256;
/// Longest recording.
pub const MAX_RECORD_MS: u32 = 10 * 60_000;
/// Most changes one recording keeps (it stops when full).
pub const MAX_RECORDED: usize = 16_384;
/// Most inputs in one [`Response::Recorded`] (well inside [`MAX_FRAME`]).
pub const MAX_RECORD_PAGE: usize = 1_500;
/// Most characters one [`Request::TypeText`] types (70 ms each: under the client's 30 s reply timeout).
pub const MAX_TYPE_TEXT: usize = 250;
/// Most results of one [`Request::Find`].
pub const MAX_FIND: u32 = 1_000;
/// Most matching lines from one [`Request::Grep`].
pub const MAX_GREP: u32 = 500;
/// Files bigger than this are skipped when grepping a folder.
pub const MAX_GREP_FILE: u64 = 16 << 20;
/// Longest line [`Request::Grep`] returns (longer ones are cut).
pub const MAX_GREP_LINE: usize = 300;

/// One change in the physical controllers' state, while recording: from `at_ms` (since the recording
/// began) they're in this state until the next change.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordedInput {
    pub at_ms: u32,
    /// [`Button::bit`]s held.
    pub buttons: u32,
    /// Sticks, each axis -[`STICK_MAX`]..=[`STICK_MAX`] (x right, y up).
    pub left: (i32, i32),
    pub right: (i32, i32),
}

/// A line [`Request::Grep`] found.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    /// From 1.
    pub line: u32,
    pub text: String,
}

/// Longest lease of one [`Request::Hold`].
pub const MAX_HOLD_LEASE_MS: u32 = 5_000;

impl Request {
    /// What the request is doing, in plain words, for a person watching ("looking at the screen").
    pub fn describe(&self) -> &'static str {
        match self {
            Request::Hello { .. } | Request::Pair { .. } => "connecting",
            Request::Unpair => "unpairing a computer",
            Request::Ping { .. }
            | Request::Status
            | Request::AppStatus
            | Request::AppletStatus
            | Request::FatalReports
            | Request::Activity
            | Request::Agents
            | Request::Introduce { .. } => "checking on the console",
            Request::ListDir { .. } | Request::Stat { .. } | Request::ReadFile { .. } => "reading files",
            Request::UploadBegin { .. } | Request::UploadChunk { .. } | Request::UploadFinish { .. } | Request::UploadAbort => {
                "copying a file to the SD card"
            }
            Request::Delete { .. } => "deleting a file",
            Request::SysmoduleStatus | Request::SysmoduleStart | Request::SysmoduleStop | Request::Handover => {
                "managing the background service"
            }
            Request::Exit => "closing Kickstand",
            Request::RebootToHekate { .. } => "restarting the console",
            Request::Press { .. }
            | Request::Input { .. }
            | Request::Hold { .. }
            | Request::Play { .. }
            | Request::TypeText { .. } => "pressing buttons",
            Request::RecordStart { .. } | Request::RecordStop | Request::RecordRead { .. } => "recording the controller",
            Request::Find { .. } | Request::Grep { .. } => "reading files",
            Request::Playback { .. } => "controlling a macro",
            Request::UsbOpen | Request::UsbClose => "connecting",
            Request::KeepAwake { .. } => "keeping the screen on",
            Request::IndicatorPosition { .. } => "placing the indicator",
            Request::Screenshot | Request::ScreenshotRead { .. } => "looking at the screen",
            Request::ListApps { .. } => "looking at installed software",
            Request::CloseApp | Request::CloseApplet => "closing software",
            Request::HblOverride { .. } => "opening homebrew",
            Request::Resources => "checking memory use",
            Request::IramPeek { .. }
            | Request::MemoryMap
            | Request::ReadMemory { .. }
            | Request::WriteMemory { .. }
            | Request::MemoryTarget { .. }
            | Request::Threads
            | Request::Profile { .. } => "debugging the running software",
            Request::AudioReplay { .. }
            | Request::AudioRead { .. }
            | Request::AudioStreamChunk { .. }
            | Request::AudioCapture
            | Request::AudioStream { .. }
            | Request::AudioControl { volume: None, muted: None } => "listening to audio",
            Request::VideoStream { .. } => "looking at the screen",
            Request::CaptureHold { .. } => "pausing gameplay capture",
            Request::AudioControl { .. } => "changing audio volume",
        }
    }
}

/// What every client does all the time ([`Request::describe`]), so not worth a line in
/// [`ActivityReport::recent`] or a warning to another agent.
pub const ROUTINE: [&str; 2] = ["connecting", "checking on the console"];

/// What a client does without changing anything on the console ([`Request::describe`]).
pub const LOOKING: [&str; 8] = [
    "reading files",
    "looking at the screen",
    "looking at installed software",
    "checking memory use",
    "keeping the screen on",
    "placing the indicator",
    "recording the controller",
    "listening to audio",
];

/// Whether a request described as `doing` changes something on the console (input, launching or closing,
/// files, memory, restarting): what another agent could clash with.
pub fn changes_console(doing: &str) -> bool {
    !ROUTINE.contains(&doing) && !LOOKING.contains(&doing)
}

/// Digits in a pairing code.
pub const PAIR_CODE_LEN: usize = 6;
/// Wrong codes in a row after which the console shows a new one.
pub const PAIR_MAX_TRIES: u32 = 5;
/// Most tokens a console keeps (one per paired PC).
pub const MAX_TOKENS: usize = 16;

/// Whether `token` may be stored: 16 to 128 ASCII letters, digits, `-` or `_` (one per line in
/// [`path::TOKEN_PATH`]).
pub fn valid_token(token: &str) -> bool {
    (16..=128).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
    Data {
        #[serde(with = "bytes")]
        data: Vec<u8>,
        file_size: u64,
    },
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
    /// (20)
    Threads { app: App, threads: Vec<ThreadInfo> },
    /// (22)
    Activity(ActivityReport),
    /// (24)
    Agents(AgentsReport),
    /// (26) Answer to [`Request::RecordStop`]: how many inputs were recorded over `ms`, and whether it
    /// stopped early because [`MAX_RECORDED`] were.
    Recording { total: u32, ms: u32, full: bool },
    /// (26)
    Recorded { inputs: Vec<RecordedInput> },
    /// (26) Answer to [`Request::Find`]: full paths in `name`.
    Found { entries: Vec<DirEntry>, truncated: bool },
    /// (26) Answer to [`Request::Grep`].
    Matches { matches: Vec<GrepMatch>, files: u32, truncated: bool },
    /// (27) Answer to [`Request::Playback`], and to a [`Request::Play`] that was stopped.
    Playback(Playback),
    /// (28) Answer to [`Request::UsbOpen`]: the USB device to open has this serial number.
    UsbOpened { serial: String },
    /// (29) Answer to [`Request::Profile`].
    Profile { app: App, elapsed_ms: u32, ticks: u32, samples: Vec<ProfileSample> },
    /// (29) Answer to [`Request::AudioControl`].
    AudioStatus(AudioOutStatus),
    /// (30) Answer to [`Request::AudioCapture`].
    AudioCapture(AudioCapture),
    /// (30) Answer to [`Request::AudioStream`].
    AudioChunk(AudioChunk),
    /// (30) Answer to [`Request::VideoStream`].
    VideoChunk(VideoChunk),
}

/// Audio capture (protocol 30). The sysmodule reads the foreground application's sound from `grc:d`, the
/// gameplay recorder (48 kHz, 16-bit, stereo, interleaved little-endian), into a ring of the last
/// [`RING_MS`](audio::RING_MS) while an application runs. Only applications that record gameplay can be
/// heard: a game that allows video capture (holding the Capture button saves a clip), or homebrew running
/// in such a game's place; a host whose control data says Manual needs the homebrew to call
/// `appletInitializeGamePlayRecording()`.
pub mod audio {
    pub const SAMPLE_RATE: u32 = 48_000;
    pub const CHANNELS: u16 = 2;
    pub const BYTES_PER_SECOND: u32 = SAMPLE_RATE * CHANNELS as u32 * 2;
    /// What the console keeps.
    pub const RING_MS: u32 = 6_000;
    /// The longest replay: less than the ring, so a replay being read isn't overwritten.
    pub const REPLAY_MAX_MS: u32 = 5_000;
    /// The longest [`super::Request::AudioStream`] wait.
    pub const MAX_WAIT_MS: u32 = 2_000;
    /// The program id of SysDVR's sysmodule, which reads the same streams.
    pub const SYSDVR_PROGRAM_ID: u64 = 0x00FF_0000_A53B_B665;

    /// Bytes in `ms` of audio, whole frames.
    pub const fn bytes_for_ms(ms: u32) -> u64 {
        (ms as u64 * SAMPLE_RATE as u64 / 1000) * CHANNELS as u64 * 2
    }

    /// Milliseconds in `bytes` of audio.
    pub const fn ms_for_bytes(bytes: u64) -> u64 {
        bytes * 1000 / BYTES_PER_SECOND as u64
    }
}

/// Video streaming (protocol 30): see [`Request::VideoStream`].
pub mod video {
    /// The reader stops this long after the last [`super::Request::VideoStream`].
    pub const IDLE_MS: u64 = 3_000;
    /// The longest a [`super::Request::VideoStream`] waits for a frame.
    pub const MAX_WAIT_MS: u32 = 1_000;
}

/// What the console's gameplay capture (`grc:d`) is doing, for audio and video alike.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum CaptureState {
    /// This daemon can't capture: the NRO (only the sysmodule can), or firmware before 6.0.
    Unsupported,
    /// No application is running (the home menu, or only applets).
    NoApplication,
    /// An application runs but the recorder has sent nothing for `ms`: it doesn't record gameplay (see
    /// [`audio`]), or it is suspended behind the home menu.
    Waiting { ms: u32 },
    /// Audio is arriving.
    Capturing,
    /// Another program reads the recorder (SysDVR), so Kickstand stays off it: two readers would each get
    /// part of the stream.
    Busy { holder: String },
    /// A `grc:d` call failed with `result` while doing `during`.
    Failed { result: u32, during: String },
    /// Like [`CaptureState::Waiting`], but an earlier daemon process read the recorder since the console
    /// booted and was stopped, and grc sends nothing to a restarted reader until a reboot (seen on 23.0.1).
    NeedsReboot { ms: u32 },
    /// [`Request::CaptureHold`] paused capture, and no `grc:d` Transfer is under way.
    Held,
}

/// Answer to [`Request::AudioCapture`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AudioCapture {
    pub state: CaptureState,
    /// The running application's program id (the host title when hbloader runs in its place).
    pub program_id: Option<u64>,
    /// Whether that's hbloader (hbmenu or an NRO), when Atmosphère says.
    pub hbl: Option<bool>,
    /// The running title's `VideoCapture` setting from its control data (0 off, 1 manual: the program must
    /// call `InitializeGamePlayRecording`, 2 automatic), read when it isn't being heard.
    pub video_capture: Option<u8>,
    /// How much audio the ring holds now (since the application started or its sound resumed).
    pub buffered_ms: u32,
    /// How long ago the newest audio arrived, if any has.
    pub newest_age_ms: Option<u32>,
    /// The stream position after the newest byte: bytes captured since the daemon started.
    pub cursor: u64,
}

/// Answer to [`Request::AudioStream`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AudioChunk {
    /// The stream position of `data`'s first byte.
    pub from: u64,
    #[serde(with = "bytes")]
    pub data: Vec<u8>,
    /// Bytes between the position asked for and `from` that are gone (overwritten, or from before the
    /// sound last restarted): a gap in the recording.
    pub skipped: u64,
    pub state: CaptureState,
}

/// Answer to [`Request::VideoStream`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VideoChunk {
    /// Part of one frame's H.264 NAL units (Annex B), empty if none arrived in time.
    #[serde(with = "bytes")]
    pub data: Vec<u8>,
    /// `data` starts a frame.
    pub frame_start: bool,
    /// `data` ends that frame.
    pub frame_end: bool,
    /// The recorder's timestamp for the frame.
    pub timestamp: u64,
    /// Frames dropped on the console since the last reply because the client read too slowly.
    pub dropped: u32,
    pub state: CaptureState,
}

/// One sampled thread stack from [`Request::Profile`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProfileSample {
    pub thread_id: u64,
    pub pc: u64,
    pub lr: u64,
    /// Return addresses walked from `fp` (`x29`), leaf-most caller first.
    pub frames: Vec<u64>,
}

/// System audio output state ([`Response::AudioStatus`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AudioOutStatus {
    pub sample_rate: u32,
    pub channels: u16,
    pub target: String,
    pub volume_pct: u8,
    pub muted: bool,
    pub output_active: bool,
}

/// Answer to [`Request::Activity`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ActivityReport {
    /// Longest connected first.
    pub clients: Vec<ClientInfo>,
    /// What clients did lately, newest first, at most [`MAX_RECENT`]: each time one turned to something
    /// else (checking on the console isn't counted).
    pub recent: Vec<ActivityEvent>,
    /// Seconds left on the keep-awake lease.
    pub keep_awake_s: u64,
}

/// Most entries in [`ActivityReport::recent`].
pub const MAX_RECENT: usize = 12;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub ip: [u8; 4],
    /// The connection that asked.
    pub you: bool,
    /// How long ago it connected.
    pub connected_ms: u64,
    pub requests: u32,
    /// Its last request, in plain words ([`Request::describe`]).
    pub doing: String,
    /// How long ago that was.
    pub idle_ms: u64,
}

/// Most bytes of an app's name in [`Request::Introduce`].
pub const MAX_APP_NAME: usize = 48;

/// Answer to [`Request::Agents`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentsReport {
    /// Longest connected first.
    pub agents: Vec<AgentInfo>,
    /// What clients did lately, newest first, as in [`ActivityReport::recent`].
    pub recent: Vec<AgentEvent>,
}

/// One client in [`AgentsReport`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// Numbers connections since the daemon started, from 1.
    pub id: u32,
    pub ip: [u8; 4],
    /// The connection that asked.
    pub you: bool,
    /// From [`Request::Introduce`].
    pub app: Option<String>,
    pub connected_ms: u64,
    pub requests: u32,
    /// Its last request, in plain words ([`Request::describe`]), and how long ago.
    pub doing: String,
    pub idle_ms: u64,
    /// Its last request that changed something ([`changes_console`]), and how long ago.
    pub change: Option<(String, u64)>,
}

/// A client turning to something else, in [`AgentsReport::recent`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    pub ago_ms: u64,
    /// [`AgentInfo::id`].
    pub id: u32,
    pub ip: [u8; 4],
    pub app: Option<String>,
    pub what: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ActivityEvent {
    pub ago_ms: u64,
    pub ip: [u8; 4],
    pub what: String,
}

/// Most threads [`Request::Threads`] reports.
pub const MAX_THREADS: usize = 128;
/// Most return addresses per thread.
pub const MAX_FRAMES: usize = 32;

/// One thread in [`Response::Threads`]: where it is (`pc`, and `lr`, which a leaf function hasn't saved
/// yet) and the return addresses found by following the frame pointers from `fp`, innermost first.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ThreadInfo {
    pub id: u64,
    pub pc: u64,
    pub lr: u64,
    pub sp: u64,
    pub fp: u64,
    pub frames: Vec<u64>,
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

    /// The byte fields are written as bytes now, the same on the wire as the sequence they were before, so
    /// older daemons and clients still understand them.
    #[test]
    fn byte_fields_encode_as_they_did_as_sequences() {
        #[derive(Serialize, Deserialize)]
        enum Old {
            _A,
            _B,
            _C,
            _D,
            _E,
            _F,
            UploadChunk { offset: u64, data: Vec<u8> },
        }
        let data: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let new = Request::UploadChunk { offset: 7, data: data.clone() };
        // Only meaningful while UploadChunk is the seventh variant.
        assert_eq!(encode(&new).unwrap(), encode(&Old::UploadChunk { offset: 7, data: data.clone() }).unwrap());
        let Ok(Request::UploadChunk { offset: 7, data: back }) = decode::<Request>(&encode(&new).unwrap()[4..]) else { panic!() };
        assert_eq!(back, data);
    }
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

    #[test]
    fn usb_transfers_end_short_and_unpad_to_the_message() {
        for body_len in [0usize, 1, 59, 60, 61, 64, 124, 508, 1020, 2044, MAX_FRAME] {
            let body: Vec<u8> = (0..body_len).map(|i| i as u8).collect();
            let mut frame = (body_len as u32).to_le_bytes().to_vec();
            frame.extend_from_slice(&body);
            let mut transfer = frame.clone();
            usb::pad(&mut transfer);
            assert!(!transfer.len().is_multiple_of(64), "{body_len}: ends in a full packet");
            assert_eq!(usb::unpad(&transfer), Some(&body[..]), "{body_len}");
            assert!(transfer.len() <= usb::MAX_TRANSFER);
        }
        // Two frames in one transfer, a cut-off one, an oversized prefix.
        let mut two = encode(&Request::Ping { nonce: 1 }).unwrap();
        two.extend(encode(&Request::Ping { nonce: 2 }).unwrap());
        assert_eq!(usb::unpad(&two), None);
        assert_eq!(usb::unpad(&encode(&Request::Ping { nonce: 1 }).unwrap()[..5]), None);
        assert_eq!(usb::unpad(&((MAX_FRAME + 1) as u32).to_le_bytes()), None);
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
                Request::Threads => 36,
                Request::Pair { .. } => 37,
                Request::Activity => 38,
                Request::Hold { .. } => 39,
                Request::Agents => 40,
                Request::Introduce { .. } => 41,
                Request::Unpair => 42,
                Request::Play { .. } => 43,
                Request::RecordStart { .. } => 44,
                Request::RecordStop => 45,
                Request::RecordRead { .. } => 46,
                Request::TypeText { .. } => 47,
                Request::Find { .. } => 48,
                Request::Grep { .. } => 49,
                Request::Playback { .. } => 50,
                Request::UsbOpen => 51,
                Request::UsbClose => 52,
                Request::Profile { .. } => 53,
                Request::AudioReplay { .. } => 54,
                Request::AudioRead { .. } => 55,
                Request::AudioStreamChunk { .. } => 56,
                Request::AudioControl { .. } => 57,
                Request::AudioCapture => 58,
                Request::AudioStream { .. } => 59,
                Request::VideoStream { .. } => 60,
                Request::CaptureHold { .. } => 61,
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
                Response::Threads { .. } => 18,
                Response::Activity(_) => 19,
                Response::Agents(_) => 20,
                Response::Recording { .. } => 21,
                Response::Recorded { .. } => 22,
                Response::Found { .. } => 23,
                Response::Matches { .. } => 24,
                Response::Playback(_) => 25,
                Response::UsbOpened { .. } => 26,
                Response::Profile { .. } => 27,
                Response::AudioStatus(_) => 28,
                Response::AudioCapture(_) => 29,
                Response::AudioChunk(_) => 30,
                Response::VideoChunk(_) => 31,
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
            Request::Threads,
            Request::Pair { version: 0, code: s(), token: s() },
            Request::Activity,
            Request::Hold { step: InputStep::default() },
            Request::Agents,
            Request::Introduce { app: s() },
            Request::Unpair,
            Request::Play { steps: Vec::new(), lease_ms: 0 },
            Request::RecordStart { max_ms: 0 },
            Request::RecordStop,
            Request::RecordRead { from: 0 },
            Request::TypeText { text: s() },
            Request::Find { dir: s(), name: s(), max: 0 },
            Request::Grep { path: s(), needle: s(), ignore_case: false, max: 0 },
            Request::Playback { action: PlaybackAction::Query },
            Request::UsbOpen,
            Request::UsbClose,
            Request::Profile { duration_ms: 0, interval_ms: 0, thread_id: None },
            Request::AudioReplay { duration_ms: 0 },
            Request::AudioRead { offset: 0, len: 0 },
            Request::AudioStreamChunk { duration_ms: 0 },
            Request::AudioControl { volume: None, muted: None },
            Request::AudioCapture,
            Request::AudioStream { from: None, min_ms: 0, max_wait_ms: 0 },
            Request::VideoStream { restart: false },
            Request::CaptureHold { ms: 0 },
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
            Response::Memory(MemoryMap { app: app.clone(), heap: None, regions: Vec::new(), truncated: false }),
            Response::error(ErrorKind::Io, s()),
            Response::Status(ConsoleStatus { uptime_ms: 0, unlocking_ms: None, keep_awake_s: 0, application: None, applet: None }),
            Response::Threads { app: app.clone(), threads: Vec::new() },
            Response::Activity(ActivityReport { clients: Vec::new(), recent: Vec::new(), keep_awake_s: 0 }),
            Response::Agents(AgentsReport::default()),
            Response::Recording { total: 0, ms: 0, full: false },
            Response::Recorded { inputs: Vec::new() },
            Response::Found { entries: Vec::new(), truncated: false },
            Response::Matches { matches: Vec::new(), files: 0, truncated: false },
            Response::Playback(Playback::default()),
            Response::UsbOpened { serial: s() },
            Response::Profile { app: app.clone(), elapsed_ms: 0, ticks: 0, samples: Vec::new() },
            Response::AudioStatus(AudioOutStatus {
                sample_rate: 48_000,
                channels: 2,
                target: s(),
                volume_pct: 50,
                muted: false,
                output_active: true,
            }),
            Response::AudioCapture(AudioCapture {
                state: CaptureState::Capturing,
                program_id: None,
                hbl: None,
                video_capture: None,
                buffered_ms: 0,
                newest_age_ms: None,
                cursor: 0,
            }),
            Response::AudioChunk(AudioChunk { from: 0, data: Vec::new(), skipped: 0, state: CaptureState::NoApplication }),
            Response::VideoChunk(VideoChunk {
                data: Vec::new(),
                frame_start: false,
                frame_end: false,
                timestamp: 0,
                dropped: 0,
                state: CaptureState::Unsupported,
            }),
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

#[cfg(test)]
mod size_tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_full_page_of_recorded_inputs_fits_a_frame() {
        let worst = RecordedInput { at_ms: u32::MAX, buttons: u32::MAX, left: (i32::MIN, i32::MIN), right: (i32::MIN, i32::MIN) };
        let page = Response::Recorded { inputs: vec![worst; MAX_RECORD_PAGE] };
        assert!(encode(&page).unwrap().len() - 4 <= MAX_FRAME);
        let steps = vec![InputStep { buttons: Button::ALL.to_vec(), ms: u32::MAX, left: Some((0, 0)), right: Some((0, 0)), touch: Some((0, 0)) }; MAX_PLAY_STEPS];
        assert!(encode(&Request::Play { steps, lease_ms: 0 }).unwrap().len() - 4 <= MAX_FRAME);
    }
}
