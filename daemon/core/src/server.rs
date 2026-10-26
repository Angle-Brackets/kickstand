//! TCP command server: one client at a time, one request then one response.

use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, Ordering};

use nx::result::Result;
use nx::socket::net::traits::SocketCommon;
use nx::socket::net::{TcpListener, TcpStream, poll};
use nx::socket::{BsdServiceConfig, BsdSrvkind, Paralellism, PollFlags};
use nx::svc;
use proto::{DaemonKind, ErrorKind, Request, Response};

use crate::log;
use crate::session::Session;

pub struct Config<'a> {
    pub kind: DaemonKind,
    pub port: u16,
    /// Shared secret clients must send in Hello. `None` refuses every client.
    pub token: Option<&'a str>,
    /// Drop a client that sends nothing for this long, so a vanished PC can't hold the only slot.
    pub idle_timeout_ms: i32,
    /// When set, the server returns soon after this becomes true (the NRO build's exit button).
    /// Without it, waits block with no timeout.
    pub stop: Option<&'a AtomicBool>,
}

/// How often blocking waits wake to check [`Config::stop`].
const STOP_CHECK_MS: i32 = 250;

/// Socket buffers sized for a sysmodule: well under 1 MiB of transfer memory, enough for 64 KiB chunks.
pub fn socket_config() -> BsdServiceConfig {
    BsdServiceConfig {
        tcp_tx_buf_size: 0x8000,
        tcp_rx_buf_size: 0x10000,
        tcp_tx_buf_max_size: 0x20000,
        tcp_rx_buf_max_size: 0x20000,
        udp_tx_buf_size: 0x2400,
        udp_rx_buf_size: 0xA500,
        sb_efficiency: 2,
        ..Default::default()
    }
}

/// Initializes sockets. `System` (`bsd:s`) for the sysmodule, `User` (`bsd:u`) for an NRO.
pub fn init_sockets(kind: DaemonKind) -> Result<()> {
    let srv = match kind {
        DaemonKind::Sysmodule => BsdSrvkind::System,
        DaemonKind::Nro => BsdSrvkind::User,
    };
    nx::socket::initialize(srv, socket_config(), None, Paralellism::One)
}

fn stopped(config: &Config) -> bool {
    config.stop.is_some_and(|s| s.load(Ordering::Relaxed))
}

/// Serves clients until [`Config::stop`] is set (forever without it). Rebinds after network errors,
/// which happen when Wi-Fi drops or the console sleeps.
pub fn run(config: &Config) {
    let mut frame = Vec::new();
    while !stopped(config) {
        let listener = match TcpListener::bind(Ipv4Addr::UNSPECIFIED, config.port) {
            Ok(l) => l,
            Err(rc) => {
                log!("bind port {} failed: {rc}; retrying", config.port);
                let _ = svc::sleep_thread(2_000_000_000);
                continue;
            }
        };
        log!("listening on port {}", config.port);
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
            log!("client {:?} connected", addr);
            let _ = stream.set_nodelay(true);
            serve(&stream, config, &mut frame);
            log!("client disconnected");
        }
        drop(listener);
        if !stopped(config) {
            let _ = svc::sleep_thread(1_000_000_000);
        }
    }
}

fn serve(stream: &TcpStream, config: &Config, frame: &mut Vec<u8>) {
    let mut session = Session::new(config.token, config.kind);
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
        if read_exact(stream, frame).is_err() {
            return;
        }
        let resp = match proto::decode::<Request>(frame) {
            Ok(req) => session.handle(req),
            Err(_) => {
                let _ = send(stream, &Response::error(ErrorKind::BadRequest, "undecodable request"));
                return;
            }
        };
        if send(stream, &resp).is_err() {
            return;
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
