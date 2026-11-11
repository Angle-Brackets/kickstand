// SPDX-License-Identifier: GPL-3.0-only
//! Kickstand over a USB cable ([`proto::Request::UsbOpen`], framing in [`proto::usb`]). One thread, made
//! once (nx never frees a thread that exits), sleeps until a client asks for the interface, presents it
//! with `usb:ds`, waits for the PC to open it, then serves it like a TCP client in a free client slot. The
//! interface goes away when the PC is done ([`proto::Request::UsbClose`]), goes quiet, or never comes, so
//! other USB homebrew can have the port back.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use nx::result::ResultCode;
use nx::service;
use nx::service::spl::{IRandomClient, RandomService};
use nx::svc;
use proto::{Error, ErrorKind, Response, usb};
use services::usbds::{Device, Dir, Identity, PageBuf, STATE_CONFIGURED};

use crate::log;
use crate::server::{self, Config, Link};

/// How often waits wake to check for a close, the idle timeout or a stopping server.
const CHECK_NS: i64 = 250_000_000;
/// Longest the PC may take to read a reply before the link is given up.
const SEND_TIMEOUT_MS: u64 = 30_000;
/// Longest [`open`] waits for the worker to present the interface.
const OPEN_TIMEOUT_MS: u64 = 5_000;
/// `KernelResult_TimedOut`, what a wait returns when nothing was signalled.
const TIMED_OUT: u32 = 0xEA01;

/// 1 while a client waits for the worker to open the interface; the worker sleeps on it.
static WAKE: AtomicU32 = AtomicU32::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);
/// Set by [`close`]: the worker ends the interface at its next check.
static CLOSE: AtomicBool = AtomicBool::new(false);
static STATE: nx::sync::Mutex<State> = nx::sync::Mutex::new(State { want: None, outcome: None, up: None, busy: false });

struct State {
    /// The serial number the next interface presents, set by [`open`].
    want: Option<String>,
    /// The worker's answer to the last [`open`].
    outcome: Option<core::result::Result<String, Error>>,
    /// The serial number of the interface that's up.
    up: Option<String>,
    /// The worker is opening, serving or closing the interface (`usb:ds` is bound).
    busy: bool,
}

/// Starts the worker (once).
pub fn start(config: &'static Config<'static>) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = nx::thread::Builder::new().name("usb").stack_size(0x10000).spawn(move || worker(config));
    if let Err(rc) = spawned {
        STARTED.store(false, Ordering::Release);
        log!("no USB thread: {rc}");
    }
}

/// [`proto::Request::UsbOpen`]: presents the interface (if it isn't up) and answers its serial number.
pub fn open() -> Response {
    if !STARTED.load(Ordering::Acquire) {
        return Response::error(ErrorKind::Unsupported, "USB needs Kickstand's sysmodule (the NRO doesn't serve USB)");
    }
    let deadline = crate::uptime_ms() + OPEN_TIMEOUT_MS;
    loop {
        {
            let mut state = STATE.lock();
            if !state.busy {
                let Some(serial) = new_serial() else {
                    return Response::error(ErrorKind::Io, "no random number for the USB serial (csrng)");
                };
                state.want = Some(serial);
                state.outcome = None;
                state.busy = true;
                CLOSE.store(false, Ordering::Release);
                break;
            }
            if !CLOSE.load(Ordering::Acquire)
                && let Some(serial) = &state.up
            {
                return Response::UsbOpened { serial: serial.clone() };
            }
        }
        if crate::uptime_ms() > deadline {
            return Response::error(ErrorKind::Busy, "the console's USB interface didn't start in time");
        }
        let _ = svc::sleep_thread(20_000_000);
    }
    WAKE.store(1, Ordering::Release);
    // SAFETY: `WAKE` is a static u32.
    let _ = unsafe { svc::signal_to_address((&raw const WAKE).cast(), svc::SignalType::Signal, 0, 1) };
    loop {
        if let Some(outcome) = STATE.lock().outcome.take() {
            return match outcome {
                Ok(serial) => Response::UsbOpened { serial },
                Err(e) => Response::Error(e),
            };
        }
        if crate::uptime_ms() > deadline {
            return Response::error(ErrorKind::Busy, "the console's USB interface didn't start in time");
        }
        let _ = svc::sleep_thread(20_000_000);
    }
}

/// [`proto::Request::UsbClose`].
pub fn close() -> Response {
    CLOSE.store(true, Ordering::Release);
    Response::Ok
}

/// Eight hex digits from csrng, so the PC opens this console's interface and not another's.
fn new_serial() -> Option<String> {
    let mut bytes = [0u8; 4];
    service::new_service_object::<RandomService>()
        .and_then(|srv| srv.generate_random_bytes(nx::ipc::sf::Buffer::from_mut_array(&mut bytes)))
        .ok()?;
    Some(format!("{:08x}", u32::from_le_bytes(bytes)))
}

fn worker(config: &'static Config<'static>) {
    loop {
        if WAKE.load(Ordering::Acquire) == 0 {
            // SAFETY: as in `open`.
            let _ = unsafe { svc::wait_for_address((&raw const WAKE).cast(), svc::ArbitrationType::WaitIfEqual, 0, -1) };
        }
        WAKE.store(0, Ordering::Release);
        let Some(serial) = STATE.lock().want.take() else {
            STATE.lock().busy = false;
            continue;
        };
        let identity = Identity {
            vendor_id: usb::VENDOR_ID,
            product_id: usb::PRODUCT_ID,
            manufacturer: usb::MANUFACTURER,
            product: usb::PRODUCT,
            serial: &serial,
            class: usb::CLASS,
        };
        let device = match Device::open(&identity) {
            Ok(device) => device,
            Err(rc) => {
                log!("USB interface didn't start: {rc}");
                let message = format!(
                    "the console's USB port couldn't be used ({rc}): another program may have it (haze, DBI, Goldleaf, a game's USB mode); close it and try again"
                );
                let mut state = STATE.lock();
                state.busy = false;
                state.outcome = Some(Err(Error { kind: ErrorKind::Busy, message, result: Some(rc.get_value()) }));
                continue;
            }
        };
        {
            let mut state = STATE.lock();
            state.up = Some(serial.clone());
            state.outcome = Some(Ok(serial.clone()));
        }
        log!("USB interface up (serial {serial})");
        let why = serve_device(&device, config);
        STATE.lock().up = None;
        drop(device);
        STATE.lock().busy = false;
        log!("USB interface down: {why}");
    }
}

/// Waits for the PC, then serves it; says why it ended.
fn serve_device(device: &Device, config: &'static Config<'static>) -> &'static str {
    let attach_by = crate::uptime_ms() + usb::ATTACH_MS;
    loop {
        match device.state() {
            Ok(STATE_CONFIGURED) => break,
            Ok(_) => {}
            Err(_) => return "its state couldn't be read",
        }
        if CLOSE.load(Ordering::Acquire) {
            return "closed before the PC opened it";
        }
        if server::stopped(config) {
            return "the server is stopping";
        }
        if crate::uptime_ms() > attach_by {
            return "the PC didn't open it (no cable to a PC, or docked?)";
        }
        let _ = svc::wait_synchronization_one(device.state_change(), CHECK_NS);
        device.clear_state_change();
    }
    let (Some(rx), Some(tx)) = (PageBuf::new(usb::MAX_TRANSFER), PageBuf::new(usb::MAX_TRANSFER)) else {
        return "no memory for its buffers";
    };
    let Some(slot) = server::take_slot() else {
        return "all client slots are taken";
    };
    log!("USB client connected (slot {slot})");
    crate::activity::connected(slot, Ipv4Addr::UNSPECIFIED);
    let mut link = UsbLink { device, config, rx, tx, ended: "the PC went away" };
    server::serve(&mut link, config, slot, Ipv4Addr::UNSPECIFIED);
    crate::activity::disconnected(slot);
    server::free_slot(slot);
    link.ended
}

struct UsbLink<'a> {
    device: &'a Device,
    config: &'static Config<'static>,
    rx: PageBuf,
    tx: PageBuf,
    /// Why the link ended, for the log.
    ended: &'static str,
}

/// How a wait on a transfer ended.
enum Waited {
    Done,
    /// The cable was pulled, or the PC reset the device.
    Unplugged,
    /// Out of time, closed, or the server is stopping.
    GaveUp(&'static str),
}

impl UsbLink<'_> {
    /// Waits for `dir`'s transfer, watching the cable, [`close`] and `deadline_ms` meanwhile.
    fn wait(&self, dir: Dir, deadline_ms: u64) -> Waited {
        let handles = [self.device.completion(dir), self.device.state_change()];
        loop {
            // SAFETY: both handles stay open for as long as the device.
            match unsafe { svc::wait_synchronization(&handles, CHECK_NS) } {
                Ok(0) => return Waited::Done,
                Ok(_) => {
                    self.device.clear_state_change();
                    if !matches!(self.device.state(), Ok(STATE_CONFIGURED)) {
                        return Waited::Unplugged;
                    }
                }
                Err(rc) if rc.get_value() == TIMED_OUT => {}
                Err(_) => return Waited::GaveUp("waiting on the interface failed"),
            }
            if dir == Dir::Out && CLOSE.load(Ordering::Acquire) {
                return Waited::GaveUp("closed");
            }
            if server::stopped(self.config) {
                return Waited::GaveUp("the server is stopping");
            }
            if crate::uptime_ms() > deadline_ms {
                return Waited::GaveUp(match dir {
                    Dir::Out => "no request for a while",
                    Dir::In => "the PC stopped reading",
                });
            }
        }
    }

    /// Runs one transfer to the end; the bytes moved.
    fn transfer(&mut self, dir: Dir, len: usize, deadline_ms: u64) -> Option<usize> {
        let buf = if dir == Dir::Out { &mut self.rx } else { &mut self.tx };
        let urb = match self.device.post(dir, buf, len) {
            Ok(urb) => urb,
            Err(rc) => {
                log!("USB transfer didn't start: {rc}");
                self.ended = "a transfer failed to start";
                return None;
            }
        };
        match self.wait(dir, deadline_ms) {
            Waited::Done => {}
            Waited::Unplugged => {
                self.device.cancel(dir);
                self.ended = "the cable was pulled";
                return None;
            }
            Waited::GaveUp(why) => {
                self.device.cancel(dir);
                self.ended = why;
                return None;
            }
        }
        match self.device.finish(dir, urb) {
            Ok(n) => Some(n),
            Err(rc) => {
                self.ended = if rc == ResultCode::new(0x828c) { "a transfer was cancelled" } else { "a transfer failed" };
                None
            }
        }
    }
}

impl Link for UsbLink<'_> {
    fn receive(&mut self, frame: &mut Vec<u8>, _config: &Config) -> bool {
        if CLOSE.load(Ordering::Acquire) {
            self.ended = "closed";
            return false;
        }
        let capacity = self.rx.len();
        let Some(n) = self.transfer(Dir::Out, capacity, crate::uptime_ms() + usb::IDLE_MS) else { return false };
        // A full buffer means the PC sent more than one frame can be.
        let Some(body) = (n < capacity).then(|| usb::unpad(&self.rx.as_slice()[..n])).flatten() else {
            log!("USB client sent a malformed transfer ({n} bytes)");
            self.ended = "it sent a malformed transfer";
            return false;
        };
        frame.clear();
        frame.extend_from_slice(body);
        true
    }

    fn send(&mut self, mut frame: Vec<u8>) -> bool {
        usb::pad(&mut frame);
        let Some(dest) = self.tx.as_mut_slice().get_mut(..frame.len()) else {
            self.ended = "a reply was too big for a transfer";
            return false;
        };
        dest.copy_from_slice(&frame);
        self.transfer(Dir::In, frame.len(), crate::uptime_ms() + SEND_TIMEOUT_MS) == Some(frame.len())
    }
}
