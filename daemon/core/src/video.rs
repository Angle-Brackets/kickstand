// SPDX-License-Identifier: GPL-3.0-only
//! Gameplay video on demand (sysmodule only): while a client keeps sending `VideoStream`, a thread reads
//! H.264 frames from `grc:d` ([`services::grc`]) into a small queue the client drains; nothing is kept once
//! it stops asking. The queue holds at most [`QUEUE_BYTES`]: when the client falls behind, queued frames are
//! dropped and the stream resumes at the next keyframe, so what the client gets always decodes.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nx::sync::Mutex;
use nx::{svc, thread};
use proto::{CaptureState, ErrorKind, MAX_CHUNK, Response, VideoChunk, video};
use services::grc::{self, Recorder};

use crate::{log, uptime_ms};

/// Frames waiting for the client, at most (a keyframe is ~100-200 KiB, others a few KiB).
const QUEUE_BYTES: usize = 384 * 1024;

/// The reader thread exists. It lives as long as the process (a detached thread that exits isn't freed:
/// one per stream used up the system's thread limit), parked while nobody wants video.
static STARTED: AtomicBool = AtomicBool::new(false);
/// The reader runs until this uptime; every `VideoStream` pushes it on.
static WANTED_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
/// Inside a grc:d Transfer (see [`crate::audio::hold`]).
static IN_TRANSFER: AtomicBool = AtomicBool::new(false);

/// Whether the reader is inside a grc:d Transfer now.
pub fn transferring() -> bool {
    IN_TRANSFER.load(Ordering::SeqCst)
}

struct Frame {
    data: Vec<u8>,
    timestamp: u64,
}

struct Queue {
    frames: VecDeque<Frame>,
    bytes: usize,
    dropped: u32,
    /// Drop frames until a keyframe (at the start, and after dropping some).
    need_keyframe: bool,
    /// Waiting for a new stream's first keyframe: what's skipped then isn't counted as dropped.
    starting: bool,
    /// The client slot reading, and when it last asked.
    owner: Option<(usize, u64)>,
    failed: Option<(u32, &'static str)>,
}

impl Queue {
    const fn new() -> Self {
        Queue { frames: VecDeque::new(), bytes: 0, dropped: 0, need_keyframe: true, starting: true, owner: None, failed: None }
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.frames.shrink_to_fit();
        self.bytes = 0;
        self.need_keyframe = true;
    }

    fn offer(&mut self, data: &[u8], timestamp: u64) {
        let keyframe = is_keyframe(data);
        if self.need_keyframe && !keyframe {
            if !self.starting {
                self.dropped += 1;
            }
            return;
        }
        if self.bytes + data.len() > QUEUE_BYTES {
            self.dropped += self.frames.len() as u32;
            self.clear();
            if !keyframe {
                self.dropped += 1;
                return;
            }
        }
        let mut copy = Vec::new();
        if copy.try_reserve_exact(data.len()).is_err() {
            self.dropped += 1;
            self.need_keyframe = true;
            return;
        }
        copy.extend_from_slice(data);
        self.need_keyframe = false;
        self.starting = false;
        self.bytes += copy.len();
        self.frames.push_back(Frame { data: copy, timestamp });
    }
}

/// Whether an Annex B access unit holds an IDR slice (5) or a sequence parameter set (7), where decoding
/// can start.
pub fn is_keyframe(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let nal_type = data[i + 3] & 0x1F;
            if nal_type == 5 || nal_type == 7 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

#[repr(C, align(4096))]
struct TransferBuffer([u8; grc::VIDEO_BUFFER_SIZE]);

fn run() {
    loop {
        // Parked: no grc:d session, no buffer.
        while uptime_ms() >= WANTED_UNTIL_MS.load(Ordering::Relaxed) {
            let _ = svc::sleep_thread(100_000_000);
        }
        read_while_wanted();
        let mut q = QUEUE.lock();
        q.clear();
        q.owner = None;
    }
}

/// Reads frames into the queue until nobody has asked for video for [`video::IDLE_MS`].
fn read_while_wanted() {
    let recorder = match Recorder::open() {
        Ok(r) => r,
        Err(rc) => {
            QUEUE.lock().failed = Some((rc.get_value(), "opening grc:d for video"));
            let _ = svc::sleep_thread(1_000_000_000);
            return;
        }
    };
    let mut buf: Option<alloc::boxed::Box<TransferBuffer>> = None;
    while uptime_ms() < WANTED_UNTIL_MS.load(Ordering::Relaxed) {
        // Don't block in grc while nothing could answer (the audio sampler knows), or while capture is held.
        if crate::audio::state_quick() != CaptureState::Capturing {
            let _ = svc::sleep_thread(100_000_000);
            continue;
        }
        let buf = match &mut buf {
            Some(b) => b,
            None => {
                // 200 KiB, only while streaming. Built in place: it's too big for this thread's stack.
                let layout = core::alloc::Layout::new::<TransferBuffer>();
                let raw = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut TransferBuffer;
                if raw.is_null() {
                    QUEUE.lock().failed = Some((0, "allocating the video buffer (sysmodule heap full)"));
                    let _ = svc::sleep_thread(1_000_000_000);
                    return;
                }
                buf.insert(unsafe { alloc::boxed::Box::from_raw(raw) })
            }
        };
        IN_TRANSFER.store(true, Ordering::SeqCst);
        let result = recorder.transfer(grc::STREAM_VIDEO, &mut buf.0);
        IN_TRANSFER.store(false, Ordering::SeqCst);
        match result {
            Ok(out) => {
                let len = (out.data_size as usize).min(grc::VIDEO_BUFFER_SIZE);
                let mut q = QUEUE.lock();
                q.failed = None;
                q.offer(&buf.0[..len], out.start_timestamp);
            }
            Err(rc) if grc::not_initialized(rc) => {
                // The audio sampler starts grc:d's stream; give it a moment.
                let _ = svc::sleep_thread(200_000_000);
            }
            Err(rc) => {
                log!("video: grc:d Transfer failed: {rc}");
                QUEUE.lock().failed = Some((rc.get_value(), "reading video from grc:d"));
                let _ = svc::sleep_thread(1_000_000_000);
            }
        }
    }
}

/// This connection's frame being sent: data, how much is sent, timestamp.
pub type Sending = Option<(Vec<u8>, usize, u64)>;

/// Answer to `VideoStream` for the client in `slot`.
pub fn stream(slot: usize, restart: bool, sending: &mut Sending) -> Response {
    if !crate::audio::started() {
        return Response::error(
            ErrorKind::Unsupported,
            "video capture runs in Kickstand's background sysmodule, not this NRO: exit the NRO (+) and try again",
        );
    }
    let now = uptime_ms();
    {
        let mut q = QUEUE.lock();
        match q.owner {
            Some((other, at)) if other != slot && now.saturating_sub(at) < video::IDLE_MS => {
                return Response::error(ErrorKind::BadRequest, "another client is streaming video from this console; try again when it stops");
            }
            _ => q.owner = Some((slot, now)),
        }
        if restart {
            q.clear();
            q.dropped = 0;
            q.starting = true;
            *sending = None;
        }
    }
    WANTED_UNTIL_MS.store(now + video::IDLE_MS, Ordering::Relaxed);
    if !STARTED.swap(true, Ordering::SeqCst) {
        // A small stack: the transfer buffer lives on the heap.
        if let Err(rc) = thread::Builder::new().name("video").stack_size(0x3000).spawn(run) {
            STARTED.store(false, Ordering::SeqCst);
            return Response::error(ErrorKind::Io, alloc::format!("couldn't start the video thread: {rc}"));
        }
    }
    if sending.is_none() {
        let deadline = now + u64::from(video::MAX_WAIT_MS);
        loop {
            {
                let mut q = QUEUE.lock();
                if let Some(frame) = q.frames.pop_front() {
                    q.bytes -= frame.data.len();
                    *sending = Some((frame.data, 0, frame.timestamp));
                    break;
                }
            }
            if uptime_ms() >= deadline {
                break;
            }
            let _ = svc::sleep_thread(10_000_000);
        }
    }
    let state = match QUEUE.lock().failed {
        Some((result, during)) => CaptureState::Failed { result, during: alloc::string::String::from(during) },
        None => crate::audio::state_quick(),
    };
    let dropped = core::mem::take(&mut QUEUE.lock().dropped);
    let Some((frame, sent, timestamp)) = sending.as_mut() else {
        return Response::VideoChunk(VideoChunk { data: Vec::new(), frame_start: false, frame_end: false, timestamp: 0, dropped, state });
    };
    let start = *sent;
    let end = (start + MAX_CHUNK).min(frame.len());
    let data = frame[start..end].to_vec();
    *sent = end;
    let timestamp = *timestamp;
    let frame_end = end == frame.len();
    if frame_end {
        *sending = None;
    }
    Response::VideoChunk(VideoChunk { data, frame_start: start == 0, frame_end, timestamp, dropped, state })
}
