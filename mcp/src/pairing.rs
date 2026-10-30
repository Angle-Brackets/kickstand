// SPDX-License-Identifier: GPL-3.0-only
//! Finding consoles on the local network ([`proto::discovery`]) and pairing with one: the user reads the
//! code off the Switch's screen, this PC sends it with its token, and saves the console's address.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use proto::discovery::{self, Announce, Probe};
use tokio::net::UdpSocket;

use crate::client::{Client, DaemonInfo};
use crate::config;

/// How long [`discover`] listens for answers by default.
pub const DISCOVER_WAIT: Duration = Duration::from_millis(1500);
/// Probes sent across the wait: Wi-Fi drops the odd broadcast.
const PROBES: u32 = 3;

/// A console that answered a probe.
#[derive(Debug, Clone)]
pub struct Found {
    /// Where its daemon listens.
    pub addr: SocketAddr,
    pub announce: Announce,
}

impl Found {
    /// "Ankit's Switch (192.168.86.191)".
    pub fn describe(&self) -> String {
        let name = if self.announce.nickname.is_empty() { "Switch" } else { &self.announce.nickname };
        format!("{name} ({})", self.addr.ip())
    }
}

/// Broadcasts probes and collects the answers for `wait`, one per console. `to` probes one address
/// instead of broadcasting (a console on another subnet, or the mock).
pub async fn discover(wait: Duration, to: Option<SocketAddr>) -> Result<Vec<Found>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.context("opening a UDP socket")?;
    socket.set_broadcast(true)?;
    let target = to.unwrap_or(SocketAddr::from((Ipv4Addr::BROADCAST, discovery::PORT)));
    let nonce: u64 = rand_u64()?;
    let probe = discovery::encode_probe(&Probe { nonce });

    let mut found: Vec<Found> = Vec::new();
    let deadline = tokio::time::Instant::now() + wait;
    let mut next_probe = tokio::time::Instant::now();
    let mut sent = 0;
    let mut buf = [0u8; discovery::MAX_DATAGRAM];
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        if sent < PROBES && now >= next_probe {
            // A network without a broadcast route fails here; the earlier answers still count.
            if let Err(e) = socket.send_to(&probe, target).await
                && sent == 0
            {
                return Err(e).with_context(|| format!("sending a discovery probe to {target}"));
            }
            sent += 1;
            next_probe = now + wait / PROBES;
        }
        let until = if sent < PROBES { next_probe.min(deadline) } else { deadline };
        let Ok(received) = tokio::time::timeout_at(until, socket.recv_from(&mut buf)).await else { continue };
        let (len, from) = received?;
        let Some(announce) = discovery::decode_announce(&buf[..len]).filter(|a| a.nonce == nonce) else { continue };
        let addr = SocketAddr::new(from.ip(), announce.port);
        if !found.iter().any(|f| f.addr == addr) {
            found.push(Found { addr, announce });
        }
    }
    found.sort_by_key(|f| f.addr);
    Ok(found)
}

/// Picks the console to pair with: `host` if given, else the one console on the network showing a code.
pub async fn pick(host: Option<&str>, port: u16) -> Result<String> {
    if let Some(host) = host {
        return Ok(host.to_string());
    }
    let found = discover(DISCOVER_WAIT, None).await?;
    let pairing: Vec<&Found> = found.iter().filter(|f| f.announce.pairing).collect();
    match pairing.as_slice() {
        [one] => Ok(one.addr.ip().to_string()),
        [] if found.is_empty() => bail!(
            "no Switch running Kickstand answered on this network. Open Kickstand from hbmenu on the Switch, and \
             check it's on the same Wi-Fi as this PC (or pass --host <its IP>)"
        ),
        [] => bail!(
            "found {} but none is showing a pairing code: open Kickstand from hbmenu on the Switch",
            found.iter().map(Found::describe).collect::<Vec<_>>().join(", ")
        ),
        many => bail!(
            "more than one Switch is showing a pairing code ({}): pass --host <IP> to pick one (port {port})",
            many.iter().map(|f| f.describe()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Pairs with the console at `host` using `code`, then saves it as the console to use.
pub async fn pair(host: &str, port: u16, code: &str) -> Result<DaemonInfo> {
    let (token, _) = config::ensure_token()?;
    let client = Client::new(format!("{host}:{port}"), token);
    let info = client.pair(code).await?;
    config::save_host(host)?;
    Ok(info)
}

fn rand_u64() -> Result<u64> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(u64::from_le_bytes(bytes))
}
