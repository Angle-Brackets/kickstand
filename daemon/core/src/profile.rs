// SPDX-License-Identifier: GPL-3.0-only
//! Sampling CPU profiler for the running application (or applet).
//!
//! Periodically attaches to the target process (`svcDebugActiveProcess`), pauses it (`svcBreakDebugProcess`),
//! reads `pc`, `lr`, and `fp` for each thread (or a single requested thread), walks the AArch64 frame-pointer
//! chain, and immediately drops the debug handle so the process resumes within ~0.5–1 ms.
//!
//! Memory usage is strictly bounded by [`MAX_SAMPLES`] and [`MAX_TOTAL_WORDS`] so the collected samples stay
//! well under 64 KiB on the sysmodule's heap and fit inside a single protocol frame.

use alloc::format;
use alloc::vec::Vec;

use nx::svc;
use proto::{App, ErrorKind, Response};
use services::debug::Debugged;

use crate::{log, uptime_ms};

/// Minimum profiling duration in milliseconds.
pub const MIN_DURATION_MS: u32 = 100;
/// Maximum profiling duration in milliseconds (keeps the request well inside the client's 30 s reply timeout).
pub const MAX_DURATION_MS: u32 = 15_000;
/// Default profiling duration in milliseconds (3 seconds).
pub const DEFAULT_DURATION_MS: u32 = 3_000;

/// Minimum sampling interval in milliseconds (100 Hz).
pub const MIN_INTERVAL_MS: u32 = 10;
/// Maximum sampling interval in milliseconds (5 Hz).
pub const MAX_INTERVAL_MS: u32 = 200;
/// Default sampling interval in milliseconds (20 ms = 50 Hz).
pub const DEFAULT_INTERVAL_MS: u32 = 20;

/// Maximum frame-pointer return addresses unwound per thread sample.
pub const MAX_FRAMES: usize = 24;
/// Maximum thread samples collected in a single profile run.
pub const MAX_SAMPLES: usize = 500;
/// Maximum total return-address `u64` words across all samples (`4096 * 8 = 32 KiB`).
pub const MAX_TOTAL_WORDS: usize = 4_096;

/// One thread's sampled call stack at a single tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub thread_id: u64,
    pub pc: u64,
    pub lr: u64,
    /// Return addresses found by following the AArch64 frame-pointer chain from `fp`, innermost caller first.
    pub frames: Vec<u64>,
}

/// Collected profile data from a console-side sampling run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileData {
    pub app: App,
    /// Actual wall-clock duration of the sampling run in milliseconds.
    pub elapsed_ms: u32,
    /// Number of sampling ticks completed.
    pub ticks: u32,
    /// Samples collected across all ticks.
    pub samples: Vec<Sample>,
}

/// Samples the running application (or the Album's process when `applet` is true) for `duration_ms`
/// at `interval_ms` intervals. When `thread_id` is `Some(id)`, only that thread is sampled.
pub fn sample(
    applet: bool,
    duration_ms: u32,
    interval_ms: u32,
    thread_id: Option<u64>,
) -> Result<ProfileData, Response> {
    let duration_ms = if duration_ms == 0 {
        DEFAULT_DURATION_MS
    } else {
        duration_ms.clamp(MIN_DURATION_MS, MAX_DURATION_MS)
    };
    let interval_ms = if interval_ms == 0 {
        DEFAULT_INTERVAL_MS
    } else {
        interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
    };

    let found = running_app(applet)?;
    let app = App { pid: found.pid, program_id: found.program_id, hbl: found.hbl };
    let pid = app.pid;

    let max_ticks = ((duration_ms / interval_ms) as usize).saturating_add(1).min(MAX_SAMPLES);
    let mut samples = Vec::new();
    let _ = samples.try_reserve(max_ticks.min(128));
    let mut total_words = 0usize;
    let mut ticks = 0u32;

    let start = uptime_ms();
    log!("profile: pid {pid}, duration {duration_ms} ms, interval {interval_ms} ms, thread {thread_id:?}");

    loop {
        if samples.len() >= MAX_SAMPLES || total_words >= MAX_TOTAL_WORDS {
            break;
        }

        // Attach and pause only long enough to read registers and walk frame pointers (~0.5-1 ms).
        let process = match Debugged::attach(pid) {
            Ok(p) => p,
            Err(rc) => {
                if !samples.is_empty() {
                    break;
                }
                return Err(system_error("attaching to the application as its debugger", rc));
            }
        };
        if let Err(rc) = process.pause() {
            drop(process);
            if !samples.is_empty() {
                break;
            }
            return Err(system_error("pausing the process", rc));
        }

        match thread_id {
            Some(id) => {
                if let Some(s) = sample_one_thread(&process, id, &mut total_words) {
                    samples.push(s);
                } else if ticks == 0 {
                    // Check whether the requested thread ID exists at all on the first tick.
                    let exists = process
                        .threads(proto::MAX_THREADS)
                        .map(|ids| ids.contains(&id))
                        .unwrap_or(false);
                    if !exists {
                        drop(process);
                        return Err(Response::error(
                            ErrorKind::NotFound,
                            format!("no thread {id:#x} in pid {pid}"),
                        ));
                    }
                }
            }
            None => {
                let ids = match process.threads(proto::MAX_THREADS) {
                    Ok(ids) => ids,
                    Err(rc) => {
                        drop(process);
                        if !samples.is_empty() {
                            break;
                        }
                        return Err(system_error("listing the threads", rc));
                    }
                };
                for id in ids {
                    if samples.len() >= MAX_SAMPLES || total_words >= MAX_TOTAL_WORDS {
                        break;
                    }
                    if let Some(s) = sample_one_thread(&process, id, &mut total_words) {
                        samples.push(s);
                    }
                }
            }
        }

        // Immediately detach and unpause the process before sleeping.
        drop(process);
        ticks += 1;

        if samples.len() >= MAX_SAMPLES || total_words >= MAX_TOTAL_WORDS {
            break;
        }
        let elapsed = uptime_ms().saturating_sub(start);
        if elapsed >= u64::from(duration_ms) {
            break;
        }
        let remaining = u64::from(duration_ms) - elapsed;
        let sleep_ms = u64::from(interval_ms).min(remaining);
        let _ = svc::sleep_thread((sleep_ms * 1_000_000) as i64);
    }

    let elapsed_ms = (uptime_ms().saturating_sub(start)).min(u64::from(u32::MAX)) as u32;
    log!("profile: done in {elapsed_ms} ms ({ticks} ticks, {} samples, {total_words} frame words)", samples.len());

    Ok(ProfileData { app, elapsed_ms, ticks, samples })
}

/// Reads one paused thread's `pc`, `lr`, and up to [`MAX_FRAMES`] frame-pointer return addresses.
fn sample_one_thread(process: &Debugged, id: u64, total_words: &mut usize) -> Option<Sample> {
    let regs = process.registers(id).ok()?;
    let max_depth = MAX_FRAMES.min(MAX_TOTAL_WORDS.saturating_sub(*total_words));
    let mut frame_buf = [0u64; MAX_FRAMES];
    let mut depth = 0usize;
    let mut fp = regs.fp;

    // Each AArch64 frame record is (caller's fp, return address) at [fp, fp + 8];
    // the stack grows down, so a caller's frame record is always strictly higher in memory.
    while depth < max_depth && fp != 0 && fp % 8 == 0 {
        let Some((next, ret)) = read_frame_record(process, fp) else { break };
        if ret == 0 {
            break;
        }
        frame_buf[depth] = ret;
        depth += 1;
        if next <= fp {
            break;
        }
        fp = next;
    }

    let mut frames = Vec::new();
    if depth > 0 {
        if frames.try_reserve_exact(depth).is_ok() {
            frames.extend_from_slice(&frame_buf[..depth]);
            *total_words += depth;
        }
    }

    Some(Sample { thread_id: id, pc: regs.pc, lr: regs.lr, frames })
}

/// Reads `(caller_fp, return_addr)` at `fp` in a single 16-byte debug read, falling back to two
/// 8-byte reads if `fp` straddles an unmapped page boundary.
fn read_frame_record(process: &Debugged, fp: u64) -> Option<(u64, u64)> {
    let mut pair = [0u8; 16];
    if process.read(fp, &mut pair).is_ok() {
        let next = u64::from_le_bytes([pair[0], pair[1], pair[2], pair[3], pair[4], pair[5], pair[6], pair[7]]);
        let ret = u64::from_le_bytes([pair[8], pair[9], pair[10], pair[11], pair[12], pair[13], pair[14], pair[15]]);
        return Some((next, ret));
    }
    let word = |address: u64| {
        let mut bytes = [0u8; 8];
        process.read(address, &mut bytes).ok().map(|()| u64::from_le_bytes(bytes))
    };
    Some((word(fp)?, word(fp.checked_add(8)?)?))
}

fn running_app(applet: bool) -> Result<services::pm::Application, Response> {
    let found = if applet { services::pm::album() } else { services::pm::application() };
    match found {
        Ok(Some(app)) => Ok(app),
        Ok(None) if applet => Err(Response::error(ErrorKind::NotFound, "the Album (hbmenu in applet mode) isn't open")),
        Ok(None) => Err(Response::error(ErrorKind::NotFound, "no application is running")),
        Err(rc) => Err(system_error("finding the process (pm)", rc)),
    }
}

fn system_error(what: &str, rc: nx::result::ResultCode) -> Response {
    let message = format!("{what}: {rc}");
    Response::Error(proto::Error { kind: ErrorKind::System, message, result: Some(rc.get_value()) })
}
