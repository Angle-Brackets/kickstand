// SPDX-License-Identifier: GPL-3.0-only
//! TCP command server: up to [`MAX_CLIENTS`] clients at once, one request then one response per client,
//! so an agent can press buttons while another connection records a GIF. Each client slot has a worker
//! thread, made once and reused: nx never frees a thread that exits (its kernel object and stack stay
//! allocated), so a thread per connection exhausted the threads every system process shares after a few
//! hundred connections, and Atmosphère's fatal service aborted.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use nx::result::Result;
use nx::socket::net::traits::{Pollable, SocketCommon};
use nx::socket::net::{TcpListener, TcpStream, UdpSocket, poll};
use proto::discovery;
use nx::socket::{BsdServiceConfig, BsdSrvkind, Paralellism, PollFlags};
use nx::svc;
use nx::thread;
use proto::{DaemonKind, ErrorKind, Request, Response};

use crate::log;
use crate::session::{AfterReply, Session};

pub struct Config<'a> {
    pub kind: DaemonKind,
    pub port: u16,
    /// Drop a client that sends nothing for this long, so a vanished PC can't hold the only slot.
    pub idle_timeout_ms: i32,
    /// When set, the server returns soon after this becomes true (the NRO build's exit button), and sets
    /// it when a client sends `Exit`. Without it, waits block with no timeout.
    pub stop: Option<&'a AtomicBool>,
}

/// How often blocking waits wake to check [`Config::stop`].
const STOP_CHECK_MS: i32 = 250;

/// Most clients served at once: the MCP server, a CLI, the desktop app and a spare. Each costs a worker
/// thread with a [`CLIENT_STACK`] stack and, while it holds a screenshot, 512 KiB of heap.
pub const MAX_CLIENTS: usize = 4;
const CLIENT_STACK: usize = 0x10000;
/// Bit n set: client slot n is taken (a client is connected, or its worker didn't start).
static SLOTS: AtomicU32 = AtomicU32::new(0);
/// The address the listener last reported (0: none), for [`listening_ip`].
static LISTENING_IP: AtomicU32 = AtomicU32::new(0);

/// Hands a connection from the accept loop to slot's worker.
struct Mailbox {
    client: nx::sync::Mutex<Option<(TcpStream, (Ipv4Addr, u16))>>,
    /// 1 once a client is waiting; the worker sleeps on this address (svcWaitForAddress) while it's 0.
    ready: AtomicU32,
}

impl Mailbox {
    fn new() -> Self {
        Mailbox { client: nx::sync::Mutex::new(None), ready: AtomicU32::new(0) }
    }

    fn post(&self, stream: TcpStream, who: (Ipv4Addr, u16)) {
        *self.client.lock() = Some((stream, who));
        self.ready.store(1, Ordering::Release);
        // SAFETY: `ready` is a live, aligned u32 for as long as the mailbox (leaked, so forever).
        let _ = unsafe { svc::signal_to_address((&raw const self.ready).cast(), svc::SignalType::Signal, 0, 1) };
    }

    /// The next client, or `None` after `timeout_ns` (-1: no timeout).
    fn take(&self, timeout_ns: i64) -> Option<(TcpStream, (Ipv4Addr, u16))> {
        if self.ready.load(Ordering::Acquire) == 0 {
            // SAFETY: as in `post`. Returns at once if `post` already set it.
            let _ = unsafe {
                svc::wait_for_address((&raw const self.ready).cast(), svc::ArbitrationType::WaitIfEqual, 0, timeout_ns)
            };
        }
        self.ready.store(0, Ordering::Release);
        self.client.lock().take()
    }
}

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

/// The console's address as of the listener's last (re)bind: unlike [`console_ip`], it needs no socket,
/// which other threads may not get while the server's sessions are busy.
pub fn listening_ip() -> Option<Ipv4Addr> {
    match LISTENING_IP.load(Ordering::Relaxed) {
        0 => None,
        ip => Some(Ipv4Addr::from(ip)),
    }
}

fn stopped(config: &Config) -> bool {
    config.stop.is_some_and(|s| s.load(Ordering::Relaxed))
}

/// Serves clients until [`Config::stop`] is set (forever without it), then waits for their threads to
/// finish. Rebinds after network errors, which happen when Wi-Fi drops or the console sleeps.
pub fn run(config: &'static Config<'static>) {
    let mailboxes: &'static [Mailbox; MAX_CLIENTS] = Box::leak(Box::new(core::array::from_fn(|_| Mailbox::new())));
    // A stoppable server (the NRO) joins its workers before returning: hbloader loads the next NRO into the
    // same process, and a thread left running there executes unmapped code.
    let mut workers = Vec::new();
    for (slot, mailbox) in mailboxes.iter().enumerate() {
        let spawned = thread::Builder::new().name("client").stack_size(CLIENT_STACK).spawn(move || worker(slot, mailbox, config));
        match spawned {
            Ok(handle) => workers.push(handle),
            Err(rc) => {
                log!("no thread for client slot {slot}: {rc}");
                SLOTS.fetch_or(1 << slot, Ordering::AcqRel);
            }
        }
    }
    let nickname = nickname();
    while !stopped(config) {
        let listener = match TcpListener::bind(Ipv4Addr::UNSPECIFIED, config.port) {
            Ok(l) => l,
            Err(rc) => {
                log!("bind port {} failed: {rc}; retrying", config.port);
                let _ = svc::sleep_thread(2_000_000_000);
                continue;
            }
        };
        let ip = console_ip();
        LISTENING_IP.store(ip.map_or(0, u32::from), Ordering::Relaxed);
        match ip {
            Some(ip) => log!("listening on {ip}:{}", config.port),
            None => log!("listening on port {} (no network address yet)", config.port),
        }
        let mut probes = bind_probes(false);
        loop {
            match wait_client_or_probe(&listener, probes.as_ref(), config) {
                Ok(Some(Ready::Client)) => {}
                Ok(Some(Ready::RetryProbes)) => {
                    probes = bind_probes(true);
                    continue;
                }
                Ok(Some(Ready::Probe)) => {
                    if let Some(probes) = &probes {
                        answer_probe(probes, config, &nickname);
                    }
                    continue;
                }
                Ok(None) => break,
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
            mailboxes[slot].post(stream, who);
        }
        drop(listener);
        if !stopped(config) {
            let _ = svc::sleep_thread(1_000_000_000);
        }
    }
    // Workers see the stop within STOP_CHECK_MS and finish their request in flight (a press, a chunk).
    for worker in workers {
        let _ = worker.join();
    }
}

/// Serves slot's clients one after another, until the server stops.
fn worker(slot: usize, mailbox: &Mailbox, config: &Config) {
    let timeout_ns = if config.stop.is_some() { i64::from(STOP_CHECK_MS) * 1_000_000 } else { -1 };
    while !stopped(config) {
        let Some((stream, who)) = mailbox.take(timeout_ns) else { continue };
        crate::activity::connected(slot, who.0);
        serve(&stream, config, slot, who.0);
        crate::activity::disconnected(slot);
        drop(stream);
        log!("client {}:{} disconnected", who.0, who.1);
        free_slot(slot);
    }
}

fn serve(stream: &TcpStream, config: &Config, slot: usize, who: Ipv4Addr) {
    let mut session = Session::new(config.kind, slot, who);
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
            // Most likely a request appended after this daemon's version: say so and carry on (the frame's
            // length kept us in step).
            Err(_) => Response::error(
                ErrorKind::Unsupported,
                alloc::format!("this daemon (protocol {}) doesn't know that request: update the sysmodule", proto::PROTOCOL_VERSION),
            ),
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
enum Ready {
    Client,
    Probe,
    /// Time to try binding the probe socket again (only without one).
    RetryProbes,
}

/// How often binding the discovery socket is retried after it failed.
const PROBE_RETRY_MS: u64 = 10_000;

/// Binds the discovery socket, or logs why not (once per run of failures).
fn bind_probes(failed_before: bool) -> Option<UdpSocket> {
    match UdpSocket::bind(Ipv4Addr::UNSPECIFIED, Some(discovery::PORT)) {
        Ok(socket) => {
            if failed_before {
                log!("discovery on (UDP {})", discovery::PORT);
            }
            Some(socket)
        }
        Err(rc) => {
            if !failed_before {
                // Without it the console just can't be found; connecting by address still works.
                log!("discovery off: binding UDP {} failed: {rc}; retrying every {} s", discovery::PORT, PROBE_RETRY_MS / 1000);
            }
            None
        }
    }
}

/// Waits for a connection on `listener` or a datagram on `probes`; `None` once the server is stopping.
/// Without `probes`, returns [`Ready::RetryProbes`] every [`PROBE_RETRY_MS`].
fn wait_client_or_probe(listener: &TcpListener, probes: Option<&UdpSocket>, config: &Config) -> Result<Option<Ready>> {
    let mut fds = alloc::vec![(RawFd(listener.get_poll_fd()), PollFlags::PollIn())];
    fds.extend(probes.map(|p| (RawFd(p.get_poll_fd()), PollFlags::PollIn())));
    let retry_at = probes.is_none().then(|| crate::uptime_ms() + PROBE_RETRY_MS);
    let step = match (config.stop.is_some(), retry_at.is_some()) {
        (true, _) => Some(STOP_CHECK_MS),
        (false, true) => Some(PROBE_RETRY_MS as i32),
        (false, false) => None,
    };
    loop {
        if stopped(config) {
            return Ok(None);
        }
        let mut ready = poll(&fds, step)?.map(|(i, _)| i).collect::<Vec<_>>();
        // A waiting client first; the probe is still there next time.
        ready.sort_unstable();
        match ready.first() {
            Some(0) => return Ok(Some(Ready::Client)),
            Some(_) => return Ok(Some(Ready::Probe)),
            None if retry_at.is_some_and(|at| crate::uptime_ms() >= at) => return Ok(Some(Ready::RetryProbes)),
            None => {}
        }
    }
}

/// Answers one discovery probe ([`proto::discovery`]) with where and what this daemon is.
fn answer_probe(probes: &UdpSocket, config: &Config, nickname: &str) {
    let mut buf = [0u8; discovery::MAX_DATAGRAM];
    let Ok((len, ip, port)) = probes.recv_from(&mut buf) else { return };
    let Some(probe) = discovery::decode_probe(&buf[..len]) else { return };
    let announce = discovery::Announce {
        nonce: probe.nonce,
        protocol: proto::PROTOCOL_VERSION,
        daemon_version: alloc::string::String::from(crate::VERSION),
        kind: config.kind,
        port: config.port,
        nickname: alloc::string::String::from(nickname),
        pairing: crate::pairing::code().is_some(),
    };
    if let Err(rc) = probes.send_to(&discovery::encode_announce(&announce), (ip, port)) {
        log!("answering a discovery probe from {ip} failed: {rc}");
    }
}

/// The console's nickname, cut to what an announce carries.
fn nickname() -> alloc::string::String {
    let mut name = services::set::nickname().unwrap_or_else(|rc| {
        log!("no console nickname ({rc})");
        alloc::string::String::new()
    });
    while name.len() > discovery::MAX_NICKNAME {
        name.pop();
    }
    name
}

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
