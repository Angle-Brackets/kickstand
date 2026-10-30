// SPDX-License-Identifier: GPL-3.0-only
//! TCP command server: up to [`MAX_CLIENTS`] clients at once, each on its own thread, one request then one
//! response per client. So an agent can press buttons while another connection records a GIF.

use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use nx::result::Result;
use nx::socket::net::traits::SocketCommon;
use nx::socket::net::{TcpListener, TcpStream, UdpSocket, poll};
use nx::socket::{BsdServiceConfig, BsdSrvkind, Paralellism, PollFlags};
use nx::svc;
use nx::thread;
use proto::{DaemonKind, ErrorKind, Request, Response};

use crate::log;
use crate::session::{AfterReply, Session};

pub struct Config<'a> {
    pub kind: DaemonKind,
    pub port: u16,
    /// Shared secret clients must send in Hello. `None` refuses every client.
    pub token: Option<&'a str>,
    /// Drop a client that sends nothing for this long, so a vanished PC can't hold the only slot.
    pub idle_timeout_ms: i32,
    /// When set, the server returns soon after this becomes true (the NRO build's exit button), and sets
    /// it when a client sends `Exit`. Without it, waits block with no timeout.
    pub stop: Option<&'a AtomicBool>,
}

/// How often blocking waits wake to check [`Config::stop`].
const STOP_CHECK_MS: i32 = 250;

/// Most clients served at once: the MCP server, a CLI and a spare. Each costs a thread with a
/// [`CLIENT_STACK`] stack and, while it holds a screenshot, 512 KiB of heap.
pub const MAX_CLIENTS: usize = 3;
const CLIENT_STACK: usize = 0x10000;
/// Bit n set: client slot n is taken.
static SLOTS: AtomicU32 = AtomicU32::new(0);

fn take_slot() -> Option<usize> {
    let mut taken = SLOTS.load(Ordering::Acquire);
    loop {
        let slot = (!taken).trailing_zeros() as usize;
        if slot >= MAX_CLIENTS {
            return None;
        }
        match SLOTS.compare_exchange_weak(taken, taken | 1 << slot, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(slot),
            Err(now) => taken = now,
        }
    }
}

fn free_slot(slot: usize) {
    SLOTS.fetch_and(!(1 << slot), Ordering::AcqRel);
}

/// Socket buffers sized for a sysmodule. The 256 KiB receive buffer holds the client's four pipelined
/// upload chunks; at 64 KiB uploads ran at ~2 MB/s. The transfer memory is `sb_efficiency` times one
/// socket's maximum buffers (~434 KiB): at 2, a client streaming screenshots left no room to accept a
/// second (its connection was reset), so 4 (~1.7 MiB) covers [`MAX_CLIENTS`] busy clients and the listener.
pub fn socket_config() -> BsdServiceConfig {
    BsdServiceConfig {
        tcp_tx_buf_size: 0x8000,
        tcp_rx_buf_size: 0x40000,
        tcp_tx_buf_max_size: 0x20000,
        tcp_rx_buf_max_size: 0x40000,
        udp_tx_buf_size: 0x2400,
        udp_rx_buf_size: 0xA500,
        sb_efficiency: 4,
        ..Default::default()
    }
}

/// Initializes sockets. `System` (`bsd:s`) for the sysmodule, `User` (`bsd:u`) for an NRO.
pub fn init_sockets(kind: DaemonKind) -> Result<()> {
    let srv = match kind {
        DaemonKind::Sysmodule => BsdSrvkind::System,
        DaemonKind::Nro => BsdSrvkind::User,
    };
    // nx lends each blocking socket call one bsd session: the listener, every client's wait, and the NRO's
    // nxlink output each need one, or they stall each other.
    nx::socket::initialize(srv, socket_config(), None, Paralellism::Five)
}

/// The console's own IPv4 address. Connecting a UDP socket sends nothing but makes the stack pick the
/// outgoing interface, whose address `getsockname` then reports.
pub fn console_ip() -> Option<Ipv4Addr> {
    let probe = UdpSocket::connect(Ipv4Addr::new(192, 0, 2, 1), 9).ok()?;
    let ip = Ipv4Addr::from(probe.local_addr().ok()?.addr);
    (!ip.is_unspecified()).then_some(ip)
}

fn stopped(config: &Config) -> bool {
    config.stop.is_some_and(|s| s.load(Ordering::Relaxed))
}

/// Serves clients until [`Config::stop`] is set (forever without it), then waits for their threads to
/// finish. Rebinds after network errors, which happen when Wi-Fi drops or the console sleeps.
pub fn run(config: &'static Config<'static>) {
    // A stoppable server (the NRO) joins its client threads before returning: hbloader loads the next NRO
    // into the same process, and a thread left running there executes unmapped code. The sysmodule never
    // stops, so its client threads run detached.
    let mut clients = Vec::new();
    while !stopped(config) {
        let listener = match TcpListener::bind(Ipv4Addr::UNSPECIFIED, config.port) {
            Ok(l) => l,
            Err(rc) => {
                log!("bind port {} failed: {rc}; retrying", config.port);
                let _ = svc::sleep_thread(2_000_000_000);
                continue;
            }
        };
        match console_ip() {
            Some(ip) => log!("listening on {ip}:{}", config.port),
            None => log!("listening on port {} (no network address yet)", config.port),
        }
        loop {
            match wait_readable(&listener, None, config) {
                Ok(true) => {}
                Ok(false) => break,
                Err(rc) => {
                    log!("poll on listener failed: {rc}");
                    break;
                }
            }
            let (stream, addr) = match listener.accept() {
                Ok(s) => s,
                Err(rc) => {
                    log!("accept failed: {rc}");
                    break;
                }
            };
            let who = (Ipv4Addr::from(addr.addr), u16::from_be(addr.port));
            let _ = stream.set_nodelay(true);
            let Some(slot) = take_slot() else {
                log!("client {}:{} refused: {MAX_CLIENTS} already connected", who.0, who.1);
                let msg = alloc::format!("the daemon already has {MAX_CLIENTS} clients; try again when one disconnects");
                let _ = send(&stream, &Response::error(ErrorKind::Busy, msg));
                continue;
            };
            log!("client {}:{} connected (slot {slot})", who.0, who.1);
            let spawned = thread::Builder::new().name("client").stack_size(CLIENT_STACK).spawn(move || {
                serve(&stream, config, slot);
                log!("client {}:{} disconnected", who.0, who.1);
                free_slot(slot);
            });
            match spawned {
                Ok(handle) if config.stop.is_some() => clients.push(handle),
                Ok(_detached) => {}
                Err(rc) => {
                    log!("no thread for a client: {rc}");
                    free_slot(slot);
                }
            }
        }
        drop(listener);
        if !stopped(config) {
            let _ = svc::sleep_thread(1_000_000_000);
        }
    }
    // Clients see the stop within STOP_CHECK_MS and finish their request in flight (a press, a chunk).
    for client in clients {
        let _ = client.join();
    }
}

fn serve(stream: &TcpStream, config: &Config, slot: usize) {
    let mut session = Session::new(config.token, config.kind, slot);
    let mut frame = Vec::new();
    loop {
        match wait_readable(stream, Some(config.idle_timeout_ms), config) {
            Ok(true) => {}
            Ok(false) => return,
            Err(rc) => {
                log!("poll on client failed: {rc}");
                return;
            }
        }
        let mut prefix = [0u8; 4];
        if read_exact(stream, &mut prefix).is_err() {
            return;
        }
        let Some(len) = proto::frame_len(prefix) else {
            let _ = send(stream, &Response::error(ErrorKind::BadRequest, "frame too large"));
            return;
        };
        frame.resize(len, 0);
        if read_exact(stream, &mut frame).is_err() {
            return;
        }
        let resp = match proto::decode::<Request>(&frame) {
            Ok(req) => session.handle(req),
            Err(_) => {
                let _ = send(stream, &Response::error(ErrorKind::BadRequest, "undecodable request"));
                return;
            }
        };
        if send(stream, &resp).is_err() {
            return;
        }
        match session.take_after_reply() {
            None => {}
            Some(AfterReply::Exit) => {
                if let Some(stop) = config.stop {
                    log!("exit requested by the client");
                    stop.store(true, Ordering::Relaxed);
                }
                return;
            }
            Some(AfterReply::Reboot) => {
                // Let the reply reach the client before the console goes away.
                let _ = svc::sleep_thread(500_000_000);
                if let Err(rc) = services::power::reboot_to_payload() {
                    log!("reboot to payload failed: {rc}");
                    // Don't leave the power menu's Restart pointed at our boot config, or the next boot
                    // (the user's) looking like the agent's.
                    let _ = crate::session::install_hekate(proto::HekateTarget::Menu);
                    crate::awake::unmark_agent_reboot();
                }
                return;
            }
            Some(AfterReply::RebootDirect(payload)) => {
                let _ = svc::sleep_thread(500_000_000);
                if let Err((step, err)) = services::power::reboot_to_payload_direct(&payload) {
                    log!("reboot to payload failed while {step}: {err:#x}");
                    crate::awake::unmark_agent_reboot();
                }
                return;
            }
        }
    }
}

/// Waits until `fd` is readable. Returns `Ok(false)` on timeout or when the server is stopping.
fn wait_readable<P: nx::socket::net::traits::Pollable>(
    fd: &P,
    timeout_ms: Option<i32>,
    config: &Config,
) -> Result<bool> {
    let fds = [(RawFd(fd.get_poll_fd()), PollFlags::PollIn())];
    let mut waited = 0;
    loop {
        if stopped(config) {
            return Ok(false);
        }
        let remaining = timeout_ms.map(|t| t - waited);
        if remaining.is_some_and(|r| r <= 0) {
            return Ok(false);
        }
        let step = match (config.stop.is_some(), remaining) {
            (true, Some(r)) => Some(r.min(STOP_CHECK_MS)),
            (true, None) => Some(STOP_CHECK_MS),
            (false, r) => r,
        };
        if poll(&fds, step)?.next().is_some() {
            return Ok(true);
        }
        waited += step.unwrap_or(0);
    }
}

/// Lets a bare descriptor be passed to [`poll`] without the owning type.
struct RawFd(i32);

impl nx::socket::net::traits::Pollable for RawFd {
    fn get_poll_fd(&self) -> i32 {
        self.0
    }
}

fn read_exact(stream: &TcpStream, buf: &mut [u8]) -> core::result::Result<(), ()> {
    let mut done = 0;
    while done < buf.len() {
        match stream.recv(&mut buf[done..]) {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => done += n,
        }
    }
    Ok(())
}

fn send(stream: &TcpStream, resp: &Response) -> core::result::Result<(), ()> {
    let bytes = proto::encode(resp).map_err(|_| ())?;
    let mut done = 0;
    while done < bytes.len() {
        match stream.send(&bytes[done..]) {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => done += n as usize,
        }
    }
    Ok(())
}
