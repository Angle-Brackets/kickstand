// SPDX-License-Identifier: GPL-3.0-only
//! Background audio capture (sysmodule only): a thread reads the foreground application's sound from the
//! gameplay recorder (`grc:d`, [`services::grc`]) into a ring holding the last [`audio::RING_MS`], while an
//! application runs. Replays ([`replay_begin`]) read the ring's newest part; live streams ([`stream`]) follow
//! its head by stream position. Nothing opens `audout:u` or `audin:u`, and the debugger isn't involved.
//!
//! The sampler keeps one `grc:d` session and one `pm:dmnt` session for its whole life. With no application
//! running it asks pm once a second; with one running that doesn't record gameplay, it sits blocked in
//! grc's Transfer (no CPU) until one that does comes along.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nx::sync::Mutex;
use nx::{svc, thread};
use proto::{AudioCapture, AudioChunk, CaptureState, ErrorKind, MAX_CHUNK, Response, audio};
use services::grc::{self, Recorder};

use crate::{log, uptime_ms};

const RING_BYTES: usize = audio::bytes_for_ms(audio::RING_MS) as usize;
/// Sound that stops for longer than this (the application was suspended or replaced) starts a new
/// recording, so a replay never splices two moments together.
const GAP_MS: u64 = 1_000;
/// A Transfer blocked this long means the recorder isn't sending: [`CaptureState::Waiting`].
const WAITING_AFTER_MS: u64 = 400;
/// How often the sampler checks which application runs while sound flows.
const CHECK_APP_MS: u64 = 1_000;
/// How often it checks for SysDVR.
const CHECK_SYSDVR_MS: u64 = 5_000;
/// grc's own audio buffer size: one recorder frame (1024 stereo samples, 21.3 ms).
const TRANSFER_BYTES: usize = grc::AUDIO_BUFFER_SIZE;

static STARTED: AtomicBool = AtomicBool::new(false);
/// An earlier Kickstand process read grc:d's stream since the console booted, and this one hasn't had any
/// sound yet: grc sends nothing to a restarted reader (see [`READER_MARK`]).
static LEFTOVER: AtomicBool = AtomicBool::new(false);
/// No new grc:d Transfers until this uptime (see [`hold`]).
static HOLD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
/// This process has written [`READER_MARK`].
static MARKED: AtomicBool = AtomicBool::new(false);
/// Written when a process first reads sound from grc:d: the uptime (ms) then; removed by a [`hold`] that
/// left no Transfer under way. One found at start from this boot (an uptime below now) means an earlier
/// daemon was stopped without that, most likely in the middle of a Transfer (the game was behind the Album,
/// say). On 23.0.1 grc then sends nothing more to anyone until a reboot, so a silent recorder means
/// "reboot", not "wait". A stop after a clean hold leaves grc working (verified on hardware).
const READER_MARK: &str = "/kickstand/grc-reader";
/// Whether this process has called grc:d Initialize (once per boot at most, by anyone).
static INITIALIZE_TRIED: AtomicBool = AtomicBool::new(false);
static RING: Mutex<Ring> = Mutex::new(Ring::new());
static PHASE: Mutex<Phase> = Mutex::new(Phase::Starting);
/// The last title whose control data was read, and its VideoCapture value.
static VIDEO_CAPTURE: Mutex<Option<(u64, Option<u8>)>> = Mutex::new(None);

/// What the sampler thread is doing.
#[derive(Clone)]
enum Phase {
    Starting,
    NoApplication,
    /// Inside a Transfer since this uptime.
    Transferring { since_ms: u64 },
    Busy(&'static str),
    Failed { result: u32, during: &'static str },
    /// [`hold`] paused it.
    Held,
}

/// The ring: stream positions count bytes captured since the daemon started, and never go back.
struct Ring {
    buf: Vec<u8>,
    /// The position after the newest byte.
    head: u64,
    /// Where the current recording starts (an application change or a pause in the sound restarts it).
    start: u64,
    last_push_ms: u64,
}

impl Ring {
    const fn new() -> Self {
        Ring { buf: Vec::new(), head: 0, start: 0, last_push_ms: 0 }
    }

    /// The oldest position still held.
    fn oldest(&self) -> u64 {
        self.start.max(self.head.saturating_sub(RING_BYTES as u64))
    }

    fn held(&self) -> u64 {
        self.head - self.oldest()
    }

    fn restart(&mut self) {
        self.start = self.head;
    }

    /// Appends `data` (whole frames). `false` if the ring couldn't be allocated.
    fn push(&mut self, data: &[u8], now_ms: u64) -> bool {
        if self.buf.len() != RING_BYTES {
            if self.buf.try_reserve_exact(RING_BYTES).is_err() {
                return false;
            }
            self.buf.resize(RING_BYTES, 0);
        }
        if self.last_push_ms != 0 && now_ms.saturating_sub(self.last_push_ms) > GAP_MS {
            self.restart();
        }
        let data = &data[..data.len() & !3];
        let data = &data[data.len().saturating_sub(RING_BYTES)..];
        let at = (self.head % RING_BYTES as u64) as usize;
        let first = (RING_BYTES - at).min(data.len());
        self.buf[at..at + first].copy_from_slice(&data[..first]);
        self.buf[..data.len() - first].copy_from_slice(&data[first..]);
        self.head += data.len() as u64;
        self.last_push_ms = now_ms;
        true
    }

    /// Copies `len` bytes from position `from` (which must be held) into `out`.
    fn copy(&self, from: u64, len: usize, out: &mut Vec<u8>) {
        let at = (from % RING_BYTES as u64) as usize;
        let first = (RING_BYTES - at).min(len);
        out.extend_from_slice(&self.buf[at..at + first]);
        out.extend_from_slice(&self.buf[..len - first]);
    }
}

fn set_phase(phase: Phase) {
    *PHASE.lock() = phase;
}

/// Starts the sampler thread (the sysmodule calls this once at start).
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Ok((data, _)) = crate::sd::read(READER_MARK, 0, 32) {
        let saved_at = core::str::from_utf8(&data).ok().and_then(|t| t.trim().parse::<u64>().ok());
        // Uptime starts over at a reboot, so a mark saved later than now is from before one.
        if saved_at.is_some_and(|at| at <= uptime_ms()) {
            LEFTOVER.store(true, Ordering::SeqCst);
            log!("audio: an earlier Kickstand read grc:d's stream this boot; it may send nothing more until a reboot");
        }
    }
    // The ring is allocated on the first sound, not here, so a console that never runs a recording game
    // never pays for it.
    let spawned = thread::Builder::new().name("audio").stack_size(0x4000).spawn(run);
    if let Err(rc) = spawned {
        log!("couldn't start the audio sampler: {rc}");
        set_phase(Phase::Failed { result: rc.get_value(), during: "starting the audio thread" });
    }
}

/// grc's Transfer writes through a mapped buffer; keep it page aligned like official software's.
#[repr(C, align(4096))]
struct TransferBuffer([u8; TRANSFER_BYTES]);

fn run() {
    let watcher = loop {
        match services::pm::Watcher::open() {
            Ok(w) => break w,
            Err(rc) => {
                set_phase(Phase::Failed { result: rc.get_value(), during: "opening pm:dmnt" });
                let _ = svc::sleep_thread(5_000_000_000);
            }
        }
    };
    let mut buf = alloc::boxed::Box::new(TransferBuffer([0; TRANSFER_BYTES]));
    let mut recorder: Option<Recorder> = None;
    let mut last_pid = None;
    let mut sysdvr_checked_ms = 0u64;
    let mut sysdvr = false;
    let mut logged_failure = false;
    loop {
        if held() {
            set_phase(Phase::Held);
            let _ = svc::sleep_thread(50_000_000);
            continue;
        }
        let Some(pid) = watcher.application_pid() else {
            set_phase(Phase::NoApplication);
            last_pid = None;
            let _ = svc::sleep_thread(1_000_000_000);
            continue;
        };
        let now = uptime_ms();
        if sysdvr_checked_ms == 0 || now.saturating_sub(sysdvr_checked_ms) >= CHECK_SYSDVR_MS {
            sysdvr = watcher.running(audio::SYSDVR_PROGRAM_ID);
            sysdvr_checked_ms = now;
        }
        if sysdvr {
            set_phase(Phase::Busy("SysDVR"));
            let _ = svc::sleep_thread(2_000_000_000);
            continue;
        }
        if last_pid != Some(pid) {
            RING.lock().restart();
            last_pid = Some(pid);
        }
        let rec = match &recorder {
            Some(rec) => rec,
            None => match Recorder::open() {
                Ok(rec) => recorder.insert(rec),
                Err(rc) => {
                    set_phase(Phase::Failed { result: rc.get_value(), during: "opening grc:d" });
                    let _ = svc::sleep_thread(5_000_000_000);
                    continue;
                }
            },
        };
        // Read until it's time to look at the application again.
        let round_start = uptime_ms();
        loop {
            if held() {
                break;
            }
            let since_ms = uptime_ms();
            set_phase(Phase::Transferring { since_ms });
            match rec.transfer(grc::STREAM_AUDIO, &mut buf.0) {
                Ok(out) => {
                    logged_failure = false;
                    let now = uptime_ms();
                    let len = (out.data_size as usize).min(TRANSFER_BYTES);
                    if !RING.lock().push(&buf.0[..len], now) {
                        set_phase(Phase::Failed { result: 0, during: "allocating the audio ring (sysmodule heap full)" });
                        let _ = svc::sleep_thread(5_000_000_000);
                        break;
                    }
                    if !MARKED.swap(true, Ordering::SeqCst) {
                        mark_reader(now);
                    }
                    // A long wait means the sound stopped: the application may have changed.
                    if now - since_ms > GAP_MS || now - round_start >= CHECK_APP_MS {
                        break;
                    }
                }
                Err(rc) if grc::not_initialized(rc) && !INITIALIZE_TRIED.swap(true, Ordering::SeqCst) => {
                    // Nobody has started grc:d's stream this boot (SysDVR or an earlier Kickstand would have):
                    // start it, once.
                    match rec.initialize() {
                        Ok(()) => log!("audio: started grc:d's stream"),
                        Err(rc) => {
                            log!("audio: grc:d Initialize failed: {rc}");
                            set_phase(Phase::Failed { result: rc.get_value(), during: "starting grc:d's stream" });
                            let _ = svc::sleep_thread(5_000_000_000);
                        }
                    }
                    break;
                }
                Err(rc) => {
                    if !logged_failure {
                        log!("audio: grc:d Transfer failed: {rc}");
                        logged_failure = true;
                    }
                    set_phase(Phase::Failed { result: rc.get_value(), during: "reading audio from grc:d" });
                    let _ = svc::sleep_thread(2_000_000_000);
                    break;
                }
            }
        }
    }
}

fn held() -> bool {
    uptime_ms() < HOLD_UNTIL_MS.load(Ordering::SeqCst)
}

/// How long [`hold`] waits for a Transfer under way to finish.
const HOLD_WAIT_MS: u64 = 1_500;

/// Answer to `CaptureHold`: starts no new grc:d Transfers for `ms` (0 resumes now), and waits up to
/// [`HOLD_WAIT_MS`] for the ones under way to finish, so stopping this process leaves none behind in grc. A
/// Transfer finishes within a frame while the application records; one still under way after the wait is
/// stuck until it records again (`state` is then not [`CaptureState::Held`]).
pub fn hold(ms: u32) -> AudioCapture {
    if !started() {
        return capture();
    }
    HOLD_UNTIL_MS.store(if ms == 0 { 0 } else { uptime_ms() + u64::from(ms) }, Ordering::SeqCst);
    if ms != 0 {
        let deadline = uptime_ms() + HOLD_WAIT_MS;
        while (!matches!(*PHASE.lock(), Phase::Held) || crate::video::transferring()) && uptime_ms() < deadline {
            let _ = svc::sleep_thread(10_000_000);
        }
        let quiet = matches!(*PHASE.lock(), Phase::Held) && !crate::video::transferring();
        log!("audio: capture held for {} s{}", ms / 1000, if quiet { "" } else { "; a grc:d Transfer is still under way" });
        if quiet {
            // Stopping now leaves grc fine for the next reader: don't make it say "reboot". Sound after the
            // hold (if this process lives on) marks the reader again.
            MARKED.store(false, Ordering::SeqCst);
            if let Err(e) = crate::sd::remove_if_exists(READER_MARK) {
                log!("audio: removing {READER_MARK}: {}", e.message);
            }
        }
    }
    capture()
}

/// Notes on the SD card that this boot's grc:d stream has a reader (see [`READER_MARK`]).
fn mark_reader(now: u64) {
    if LEFTOVER.swap(false, Ordering::SeqCst) {
        log!("audio: grc:d sends sound again after the restart");
    }
    let text = alloc::format!("{now}");
    let written = crate::sd::create_dirs("/kickstand").and_then(|()| {
        let mut file = crate::sd::create_for_write(READER_MARK, text.len() as u64)?;
        file.write_array::<u8, true>(text.as_bytes()).map_err(|rc| crate::sd::io_error("write", READER_MARK, rc))
    });
    if let Err(e) = written {
        log!("audio: saving {READER_MARK}: {}", e.message);
    }
}

/// Whether this daemon captures (the sysmodule, once started).
pub fn started() -> bool {
    STARTED.load(Ordering::Relaxed)
}

/// The capture's state as clients see it. Asks pm which application runs (the sampler may be blocked).
pub fn state() -> CaptureState {
    if !started() {
        return CaptureState::Unsupported;
    }
    if let Ok(None) = services::pm::application() {
        return CaptureState::NoApplication;
    }
    match state_quick() {
        CaptureState::NoApplication => CaptureState::Waiting { ms: 0 },
        state => state,
    }
}

/// [`state`] from what the sampler last said, without asking pm (for loops).
pub fn state_quick() -> CaptureState {
    if !started() {
        return CaptureState::Unsupported;
    }
    match PHASE.lock().clone() {
        Phase::Starting => CaptureState::Waiting { ms: 0 },
        Phase::NoApplication => CaptureState::NoApplication,
        Phase::Transferring { since_ms } => {
            let waited = uptime_ms().saturating_sub(since_ms);
            if waited < WAITING_AFTER_MS {
                CaptureState::Capturing
            } else if LEFTOVER.load(Ordering::Relaxed) {
                CaptureState::NeedsReboot { ms: waited as u32 }
            } else {
                CaptureState::Waiting { ms: waited as u32 }
            }
        }
        Phase::Busy(holder) => CaptureState::Busy { holder: String::from(holder) },
        Phase::Failed { result, during } => CaptureState::Failed { result, during: String::from(during) },
        Phase::Held => CaptureState::Held,
    }
}

/// Answer to `AudioCapture`.
pub fn capture() -> AudioCapture {
    let state = state();
    let app = if started() { services::pm::application().ok().flatten() } else { None };
    // The title's VideoCapture setting explains silence; read it (once per title) only when it's quiet.
    let video_capture = match (&app, &state) {
        (Some(app), CaptureState::Waiting { ms }) if *ms >= 1_000 => {
            let mut cached = VIDEO_CAPTURE.lock();
            match *cached {
                Some((id, value)) if id == app.program_id => value,
                _ => {
                    let value = services::ns::video_capture(app.program_id).ok().flatten();
                    *cached = Some((app.program_id, value));
                    value
                }
            }
        }
        _ => None,
    };
    let ring = RING.lock();
    let now = uptime_ms();
    AudioCapture {
        state,
        program_id: app.as_ref().map(|a| a.program_id),
        hbl: app.as_ref().and_then(|a| a.hbl),
        video_capture,
        buffered_ms: audio::ms_for_bytes(ring.held()) as u32,
        newest_age_ms: (ring.last_push_ms != 0).then(|| now.saturating_sub(ring.last_push_ms) as u32),
        cursor: ring.head,
    }
}

fn unsupported() -> Response {
    Response::error(
        ErrorKind::Unsupported,
        "audio capture runs in Kickstand's background sysmodule, not this NRO: exit the NRO (+) and try again",
    )
}

/// This connection's replay: positions `start..start + len`.
pub type Snapshot = Option<(u64, u64)>;

/// Answer to `AudioReplay`: marks the newest `duration_ms` held and sends its first chunk.
pub fn replay_begin(snapshot: &mut Snapshot, duration_ms: u32) -> Response {
    if !started() {
        return unsupported();
    }
    if duration_ms == 0 || duration_ms > audio::REPLAY_MAX_MS {
        return Response::error(ErrorKind::BadRequest, alloc::format!("audio replays are 1-{} ms", audio::REPLAY_MAX_MS));
    }
    let len = {
        let ring = RING.lock();
        let len = audio::bytes_for_ms(duration_ms).min(ring.held());
        *snapshot = Some((ring.head - len, len));
        len
    };
    let first = replay_read(snapshot, 0, MAX_CHUNK as u32);
    match first {
        Response::Data { data, .. } => Response::Data { data, file_size: len },
        other => other,
    }
}

/// Answer to `AudioRead`.
pub fn replay_read(snapshot: &Snapshot, offset: u64, len: u32) -> Response {
    let Some((start, total)) = *snapshot else {
        return Response::error(ErrorKind::BadRequest, "no audio replay on this connection; send AudioReplay first");
    };
    let offset = offset.min(total) & !1;
    let len = (len as u64).min(MAX_CHUNK as u64).min(total - offset) as usize & !1;
    let ring = RING.lock();
    if start + offset < ring.oldest() {
        return Response::error(
            ErrorKind::BadRequest,
            "that part of the audio replay has been overwritten by newer sound; read replays as soon as they're taken",
        );
    }
    let mut data = Vec::with_capacity(len);
    ring.copy(start + offset, len, &mut data);
    Response::Data { data, file_size: total }
}

/// Answer to `AudioStream`.
pub fn stream(from: Option<u64>, min_ms: u32, max_wait_ms: u32) -> Response {
    if !started() {
        return unsupported();
    }
    let deadline = uptime_ms() + u64::from(max_wait_ms.min(audio::MAX_WAIT_MS));
    let want = audio::bytes_for_ms(min_ms).clamp(4, MAX_CHUNK as u64 & !3);
    let asked = from.unwrap_or_else(|| RING.lock().head);
    loop {
        let state = state_quick();
        let ring = RING.lock();
        let pos = asked.min(ring.head);
        let begin = pos.max(ring.oldest());
        let available = ring.head - begin;
        if available >= want || uptime_ms() >= deadline || state != CaptureState::Capturing {
            let len = available.min(MAX_CHUNK as u64 & !3) as usize;
            let mut data = Vec::new();
            if data.try_reserve_exact(len).is_err() {
                return Response::error(ErrorKind::OutOfMemory, "no room for an audio chunk in the sysmodule's heap");
            }
            ring.copy(begin, len, &mut data);
            return Response::AudioChunk(AudioChunk { from: begin, data, skipped: begin - pos, state });
        }
        drop(ring);
        let _ = svc::sleep_thread(20_000_000);
    }
}

/// Queries the console's audio output (target, volume, mute) from `audctl`.
pub fn status() -> services::audio::AudioStatus {
    let mut st = services::audio::status().unwrap_or_default();
    st.output_active = state_quick() == CaptureState::Capturing;
    st
}

/// Sets the master volume (`0.0..=1.0`) and/or mute, returning the new state.
pub fn configure(volume: Option<f32>, muted: Option<bool>) -> Result<services::audio::AudioStatus, nx::result::ResultCode> {
    if let Some(v) = volume {
        services::audio::set_volume(v)?;
    }
    if let Some(m) = muted {
        services::audio::set_mute(m)?;
    }
    let mut st = services::audio::status()?;
    st.output_active = state_quick() == CaptureState::Capturing;
    Ok(st)
}
