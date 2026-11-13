// SPDX-License-Identifier: GPL-3.0-only
//! Writes upload data to the SD card on a thread of its own (sysmodule only), so the next chunks come in over
//! the link meanwhile and an upload takes as long as the slower of the two instead of both added up (over USB
//! the SD card is the slower: ~20 MB/s in 512 KiB writes). One thread, made once (nx never frees a thread that
//! exits), serves every client slot: each slot has one write in flight at most, and a [`Job`] carries the file
//! and the buffer there and back, so nothing is shared while it's away.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use nx::fs::FileAccessor;
use nx::svc;

use crate::log;
use crate::server::MAX_CLIENTS;

/// One write: `buf` goes to `file` at its current offset; `result` says how it went, and `buf` comes back
/// empty.
pub struct Job {
    pub file: FileAccessor,
    pub buf: Vec<u8>,
    pub result: nx::result::Result<()>,
}

const EMPTY: u32 = 0;
const QUEUED: u32 = 1;
const DONE: u32 = 2;

struct Cell {
    /// [`EMPTY`], [`QUEUED`] or [`DONE`]; the slot's session sleeps on it while it's [`QUEUED`].
    state: AtomicU32,
    job: nx::sync::Mutex<Option<Job>>,
}

static CELLS: [Cell; MAX_CLIENTS] =
    [const { Cell { state: AtomicU32::new(EMPTY), job: nx::sync::Mutex::new(None) } }; MAX_CLIENTS];
/// 1 when a job may be queued; the thread sleeps on it while it's 0.
static WORK: AtomicU32 = AtomicU32::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Starts the thread (once). Without it, uploads write each piece themselves.
pub fn start() {
    if RUNNING.load(Ordering::Acquire) {
        return;
    }
    match nx::thread::Builder::new().name("sd-writer").stack_size(0x4000).spawn(run) {
        Ok(_) => RUNNING.store(true, Ordering::Release),
        Err(rc) => log!("no SD writer thread: {rc}"),
    }
}

/// Whether [`submit`] can be used.
pub fn running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

/// Hands `job` to the thread for `slot`, which must have none in flight ([`wait`] first).
pub fn submit(slot: usize, job: Job) {
    let cell = &CELLS[slot];
    *cell.job.lock() = Some(job);
    cell.state.store(QUEUED, Ordering::SeqCst);
    WORK.store(1, Ordering::SeqCst);
    // SAFETY: WORK is a static u32.
    let _ = unsafe { svc::signal_to_address((&raw const WORK).cast(), svc::SignalType::Signal, 0, 1) };
}

/// Waits for `slot`'s write to finish and takes it back; `None` if none was submitted.
pub fn wait(slot: usize) -> Option<Job> {
    let cell = &CELLS[slot];
    loop {
        match cell.state.load(Ordering::SeqCst) {
            QUEUED => {
                // SAFETY: the cell is static. Returns at once if the state is no longer QUEUED.
                let _ = unsafe {
                    svc::wait_for_address((&raw const cell.state).cast(), svc::ArbitrationType::WaitIfEqual, QUEUED, -1)
                };
            }
            DONE => {
                let job = cell.job.lock().take();
                cell.state.store(EMPTY, Ordering::SeqCst);
                return job;
            }
            _ => return None,
        }
    }
}

fn run() {
    loop {
        if WORK.load(Ordering::SeqCst) == 0 {
            // SAFETY: WORK is a static u32. Returns at once if it's no longer 0.
            let _ = unsafe { svc::wait_for_address((&raw const WORK).cast(), svc::ArbitrationType::WaitIfEqual, 0, -1) };
        }
        WORK.store(0, Ordering::SeqCst);
        for cell in &CELLS {
            if cell.state.load(Ordering::SeqCst) != QUEUED {
                continue;
            }
            let job = cell.job.lock().take();
            if let Some(mut job) = job {
                job.result = job.file.write_array::<u8, false>(&job.buf);
                job.buf.clear();
                *cell.job.lock() = Some(job);
            }
            cell.state.store(DONE, Ordering::SeqCst);
            // SAFETY: the cell is static.
            let _ = unsafe { svc::signal_to_address((&raw const cell.state).cast(), svc::SignalType::Signal, 0, -1) };
        }
    }
}
