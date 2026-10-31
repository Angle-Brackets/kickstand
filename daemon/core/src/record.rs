// SPDX-License-Identifier: GPL-3.0-only
//! Recording the physical controllers, for macros the user plays once and agents replay: a thread reads
//! hid's shared memory every [`SAMPLE_NS`] (as any libnx homebrew reads its input) and keeps each change.
//! Every player and the handheld Joy-Cons are merged: buttons together, each stick from whichever is pushed
//! furthest. HOME and Capture aren't in that memory, so they aren't recorded. Nothing is recorded while
//! nobody asked: the hid session exists only while a recording runs.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use nx::input;
use nx::service::hid;
use nx::svc;
use nx::thread;
use proto::{MAX_RECORDED, RecordedInput, STICK_MAX};

use crate::{log, uptime_ms};

/// About two samples a frame; hid itself samples controllers every few milliseconds.
const SAMPLE_NS: i64 = 8_000_000;
/// A stick nearer the centre than this (an eighth of the way) counts as centred: a resting stick drifts.
const DEADZONE: i32 = STICK_MAX / 8;
/// Stick positions are kept in steps of this (1/20 of the way), so a hand's tremor isn't a change.
const STICK_STEP: i32 = STICK_MAX / 20;
/// `HidNpadButton` A through Down: not the stick-as-direction bits, SL/SR or the rest.
const BUTTON_MASK: u64 = 0xFFFF;

/// Set while a recording should run; the thread stops when it's cleared.
static RECORDING: AtomicBool = AtomicBool::new(false);
/// Set while the thread runs.
static RUNNING: AtomicBool = AtomicBool::new(false);
static STATE: nx::sync::Mutex<State> = nx::sync::Mutex::new(State { inputs: Vec::new(), ms: 0, full: false, error: None });

struct State {
    inputs: Vec<RecordedInput>,
    /// How long it ran, once stopped.
    ms: u32,
    full: bool,
    /// Why it couldn't record, if it couldn't.
    error: Option<nx::result::ResultCode>,
}

/// Starts recording for at most `max_ms`, throwing away a recording in progress.
pub fn start(max_ms: u32) -> Result<(), &'static str> {
    stop_thread();
    {
        let mut state = STATE.lock();
        state.inputs = Vec::new();
        state.ms = 0;
        state.full = false;
        state.error = None;
    }
    RECORDING.store(true, Ordering::SeqCst);
    RUNNING.store(true, Ordering::SeqCst);
    let spawned = thread::Builder::new().name("record").stack_size(0x8000).spawn(move || {
        run(max_ms);
        RUNNING.store(false, Ordering::SeqCst);
    });
    if spawned.is_err() {
        RECORDING.store(false, Ordering::SeqCst);
        RUNNING.store(false, Ordering::SeqCst);
        return Err("the recording thread didn't start");
    }
    Ok(())
}

/// Stops recording (if it still runs) and returns how many inputs there are, over how long, whether it
/// filled up, and why it failed if it did.
pub fn stop() -> (u32, u32, bool, Option<nx::result::ResultCode>) {
    stop_thread();
    let state = STATE.lock();
    (state.inputs.len() as u32, state.ms, state.full, state.error)
}

/// Inputs from number `from`, at most `max`.
pub fn read(from: usize, max: usize) -> Vec<RecordedInput> {
    let state = STATE.lock();
    state.inputs.iter().skip(from).take(max).copied().collect()
}

fn stop_thread() {
    RECORDING.store(false, Ordering::SeqCst);
    // A sample takes well under a millisecond, so this is short.
    for _ in 0..100 {
        if !RUNNING.load(Ordering::SeqCst) {
            return;
        }
        let _ = svc::sleep_thread(SAMPLE_NS);
    }
    log!("record: the thread didn't stop");
}

fn run(max_ms: u32) {
    let styles = hid::NpadStyleTag::FullKey()
        | hid::NpadStyleTag::Handheld()
        | hid::NpadStyleTag::JoyDual()
        | hid::NpadStyleTag::JoyLeft()
        | hid::NpadStyleTag::JoyRight();
    let ctx = match input::Context::new(styles, 8) {
        Ok(ctx) => ctx,
        Err(rc) => {
            log!("record: opening hid failed: {rc}");
            STATE.lock().error = Some(rc);
            return;
        }
    };
    let ids = [
        hid::NpadIdType::No1,
        hid::NpadIdType::No2,
        hid::NpadIdType::No3,
        hid::NpadIdType::No4,
        hid::NpadIdType::No5,
        hid::NpadIdType::No6,
        hid::NpadIdType::No7,
        hid::NpadIdType::No8,
        hid::NpadIdType::Handheld,
    ];
    let start = uptime_ms();
    log!("record: started ({} s at most)", max_ms / 1000);
    let mut last: Option<RecordedInput> = None;
    while RECORDING.load(Ordering::SeqCst) {
        let elapsed = uptime_ms() - start;
        if elapsed >= u64::from(max_ms) {
            break;
        }
        let mut now = RecordedInput { at_ms: elapsed as u32, ..Default::default() };
        for id in ids {
            let mut player = ctx.get_player(id);
            now.buttons |= (player.get_buttons().get() & BUTTON_MASK) as u32;
            let (l, r) = player.get_stick_status(player.get_reported_style_tag());
            now.left = further(now.left, quantize(l.x, l.y));
            now.right = further(now.right, quantize(r.x, r.y));
        }
        let changed = last.is_none_or(|l| (l.buttons, l.left, l.right) != (now.buttons, now.left, now.right));
        if changed {
            let mut state = STATE.lock();
            if state.inputs.len() >= MAX_RECORDED {
                state.full = true;
                break;
            }
            state.inputs.push(now);
            last = Some(now);
        }
        let _ = svc::sleep_thread(SAMPLE_NS);
    }
    let ms = (uptime_ms() - start).min(u64::from(max_ms)) as u32;
    let mut state = STATE.lock();
    state.ms = ms;
    log!("record: stopped after {} s, {} changes", ms / 1000, state.inputs.len());
    drop(state);
    RECORDING.store(false, Ordering::SeqCst);
    // `ctx` is dropped here, closing the hid session.
}

/// A stick position with the deadzone applied, in [`STICK_STEP`]s.
fn quantize(x: i32, y: i32) -> (i32, i32) {
    let (x, y) = (x.clamp(-STICK_MAX, STICK_MAX), y.clamp(-STICK_MAX, STICK_MAX));
    let magnitude2 = i64::from(x) * i64::from(x) + i64::from(y) * i64::from(y);
    if magnitude2 < i64::from(DEADZONE) * i64::from(DEADZONE) {
        return (0, 0);
    }
    let step = |v: i32| ((v + v.signum() * STICK_STEP / 2) / STICK_STEP * STICK_STEP).clamp(-STICK_MAX, STICK_MAX);
    (step(x), step(y))
}

/// Whichever of two stick positions is pushed further.
fn further(a: (i32, i32), b: (i32, i32)) -> (i32, i32) {
    let m = |(x, y): (i32, i32)| i64::from(x) * i64::from(x) + i64::from(y) * i64::from(y);
    if m(b) > m(a) { b } else { a }
}
