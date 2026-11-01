// SPDX-License-Identifier: GPL-3.0-only
//! Pausing and stopping macros ([`proto::Request::Playback`]) from any connection, while another one
//! replays it with [`proto::Request::Play`]s. A run is a connection's Plays back to back; it ends with the
//! connection's next other request. Stopping bumps a generation that every run compares with the one it
//! started at, so the Plays already on their way after a stop are refused too.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use proto::{MAX_PAUSE_MS, Playback, PlaybackAction};
use services::hid::Go;

use crate::uptime_ms;

const NOBODY: usize = usize::MAX;

static GENERATION: AtomicU32 = AtomicU32::new(0);
/// Uptime ms when paused (at least 1), or 0.
static PAUSED_SINCE: AtomicU64 = AtomicU64::new(0);
/// The client slot whose run is playing.
static PLAYER: AtomicUsize = AtomicUsize::new(NOBODY);

/// Starts a run for the client in `slot`, unpaused; returns the generation to pass to [`go`].
pub fn begin(slot: usize) -> u32 {
    PAUSED_SINCE.store(0, Ordering::SeqCst);
    PLAYER.store(slot, Ordering::SeqCst);
    GENERATION.load(Ordering::SeqCst)
}

/// Ends `slot`'s run (if it's the one playing).
pub fn end(slot: usize) {
    if PLAYER.compare_exchange(slot, NOBODY, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        PAUSED_SINCE.store(0, Ordering::SeqCst);
    }
}

/// Whether a run started at `generation` was stopped since.
pub fn stopped(generation: u32) -> bool {
    GENERATION.load(Ordering::SeqCst) != generation
}

/// What a run started at `generation` should do now: a pause over [`MAX_PAUSE_MS`] stops it.
pub fn go(generation: u32) -> Go {
    if stopped(generation) {
        return Go::Stop;
    }
    match PAUSED_SINCE.load(Ordering::SeqCst) {
        0 => Go::Play,
        since if uptime_ms().saturating_sub(since) > u64::from(MAX_PAUSE_MS) => {
            crate::log!("playback: paused too long, stopping");
            stop();
            Go::Stop
        }
        _ => Go::Pause,
    }
}

fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    PLAYER.store(NOBODY, Ordering::SeqCst);
    PAUSED_SINCE.store(0, Ordering::SeqCst);
}

/// Whether a macro plays (and who plays it) as it was when asked, after doing `action`.
pub fn control(action: PlaybackAction) -> Playback {
    let player = PLAYER.load(Ordering::SeqCst);
    let playing = player != NOBODY;
    let state = Playback {
        playing,
        paused: playing && PAUSED_SINCE.load(Ordering::SeqCst) != 0,
        app: if playing { crate::activity::clients().into_iter().find(|c| c.slot == player).and_then(|c| c.app) } else { None },
    };
    if playing {
        match action {
            PlaybackAction::Query => {}
            PlaybackAction::Pause => {
                let _ = PAUSED_SINCE.compare_exchange(0, uptime_ms().max(1), Ordering::SeqCst, Ordering::SeqCst);
            }
            PlaybackAction::Resume => PAUSED_SINCE.store(0, Ordering::SeqCst),
            PlaybackAction::Stop => {
                stop();
                // Also between two Plays, when the last step is held for the next: waits for a Play
                // running now to see the stop (within 10 ms), then lets go and detaches.
                services::hid::release_hold();
            }
        }
        let changed = match action {
            PlaybackAction::Query => false,
            PlaybackAction::Pause => !state.paused,
            PlaybackAction::Resume => state.paused,
            PlaybackAction::Stop => true,
        };
        if changed {
            crate::log!("playback: {action:?}");
        }
    }
    state
}
