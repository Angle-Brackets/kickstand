// SPDX-License-Identifier: GPL-3.0-only
//! stdout over nxlink, like libnx's `nxlinkStdio()`.
//!
//! `nxlink -s` on the PC sends the NRO to hbmenu's NetLoader, which appends `<hex addr>_NXLINK_` to the
//! NRO's argv, then waits for the NRO to connect back to port 28771 and prints whatever arrives.
//! Call [`connect`] once sockets are initialized, then use [`print!`]/[`println!`].

#![no_std]

use core::fmt::{self, Write};
use core::net::Ipv4Addr;

use nx::result::Result;
use nx::socket::net::TcpStream;
use nx::socket::net::traits::SocketCommon;
use nx::sync::Mutex;

/// The port `nxlink -s` listens on for the NRO's output.
pub const CLIENT_PORT: u16 = 28771;

const ARGV_SUFFIX: &str = "_NXLINK_";

static STDOUT: Mutex<Option<TcpStream>> = Mutex::new(None);

/// The arguments the NRO was launched with, minus the program path and nxlink's host marker.
pub fn args() -> impl Iterator<Item = &'static str> {
    let mut parts = split_argv(nx::hbl::get_argv().unwrap_or(""));
    let _program = parts.next();
    parts.filter(|part| parse_host(part).is_none())
}

/// The PC that sent this NRO, if it came through nxlink.
pub fn host() -> Option<Ipv4Addr> {
    split_argv(nx::hbl::get_argv()?).last().and_then(parse_host)
}

/// Splits HBL's argv string the way libnx does: on spaces, except inside double or single quotes. hbmenu
/// quotes every argument (`"sdmc:/switch/x.nro" "one" "6056a8c0_NXLINK_"`).
fn split_argv(argv: &str) -> impl Iterator<Item = &str> {
    let mut rest = argv;
    core::iter::from_fn(move || {
        rest = rest.trim_start_matches([' ', '\t']);
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
        let (arg, tail) = match quote {
            Some(q) => {
                let body = &rest[1..];
                body.split_once(q).unwrap_or((body, ""))
            }
            None if rest.is_empty() => return None,
            None => rest.split_once([' ', '\t']).unwrap_or((rest, "")),
        };
        rest = tail;
        Some(arg)
    })
}

/// Parses nxlink's argv marker: 8 hex digits of the address as libnx's `in_addr.s_addr`, then `_NXLINK_`.
fn parse_host(arg: &str) -> Option<Ipv4Addr> {
    let hex = arg.strip_suffix(ARGV_SUFFIX)?;
    if hex.len() != 8 {
        return None;
    }
    // s_addr holds the address in network order, so its little-endian bytes are the octets in order.
    let s_addr = u32::from_str_radix(hex, 16).ok()?;
    Some(Ipv4Addr::from(s_addr.to_le_bytes()))
}

/// Connects stdout to the nxlink host. Returns `Ok(false)` when the NRO wasn't started by nxlink.
/// Sockets must already be initialized (`nx::socket::initialize`).
/// `KICKSTAND_DEBUG_HOST` (an IPv4 address, set at build time) is used when argv has no host, to debug
/// argv itself.
pub fn connect() -> Result<bool> {
    let fallback = option_env!("KICKSTAND_DEBUG_HOST").and_then(|h| h.parse().ok());
    let Some(host) = host().or(fallback) else {
        return Ok(false);
    };
    let stream = TcpStream::connect(host, CLIENT_PORT)?;
    *STDOUT.lock() = Some(stream);
    Ok(true)
}

pub fn is_connected() -> bool {
    STDOUT.lock().is_some()
}

/// Drops the connection, which tells `nxlink -s` the program is done.
pub fn disconnect() {
    *STDOUT.lock() = None;
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut guard = STDOUT.lock();
    if let Some(stream) = guard.as_mut() {
        // A broken connection just loses output; the program keeps running.
        if stream.write_fmt(args).is_err() {
            *guard = None;
        }
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

