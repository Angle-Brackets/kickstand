// SPDX-License-Identifier: GPL-3.0-only
//! Who is connected and what they're doing, in plain words, for the NRO's screen and
//! [`Request::Activity`].

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use proto::{MAX_RECENT, Request};

use crate::server::MAX_CLIENTS;

#[derive(Clone, Copy)]
pub struct Client {
    pub slot: usize,
    pub ip: Ipv4Addr,
    pub connected_ms: u64,
    pub requests: u32,
    /// The last request, as something a person would understand.
    pub doing: &'static str,
    pub last_ms: u64,
}

/// A client turning to something else.
#[derive(Clone, Copy)]
pub struct Event {
    pub at_ms: u64,
    pub ip: Ipv4Addr,
    pub what: &'static str,
}

static CLIENTS: nx::sync::Mutex<[Option<Client>; MAX_CLIENTS]> = nx::sync::Mutex::new([None; MAX_CLIENTS]);
/// Oldest first.
static RECENT: nx::sync::Mutex<VecDeque<Event>> = nx::sync::Mutex::new(VecDeque::new());

/// Not worth a line in [`recent`]: every agent does these all the time.
const ROUTINE: [&str; 2] = ["connecting", "checking on the console"];

pub fn connected(slot: usize, ip: Ipv4Addr) {
    let now = crate::uptime_ms();
    CLIENTS.lock()[slot] = Some(Client { slot, ip, connected_ms: now, requests: 0, doing: "connecting", last_ms: now });
}

pub fn disconnected(slot: usize) {
    CLIENTS.lock()[slot] = None;
}

pub fn request(slot: usize, req: &Request) {
    let now = crate::uptime_ms();
    let doing = req.describe();
    let changed = match CLIENTS.lock()[slot].as_mut() {
        Some(client) => {
            let changed = client.doing != doing;
            client.requests += 1;
            client.doing = doing;
            client.last_ms = now;
            changed.then_some(client.ip)
        }
        None => None,
    };
    if let Some(ip) = changed
        && !ROUTINE.contains(&doing)
    {
        let mut recent = RECENT.lock();
        if recent.len() == MAX_RECENT {
            recent.pop_front();
        }
        recent.push_back(Event { at_ms: now, ip, what: doing });
    }
}

/// What clients did lately, newest first.
pub fn recent() -> Vec<Event> {
    RECENT.lock().iter().rev().copied().collect()
}

/// The connected clients, longest connected first.
pub fn clients() -> Vec<Client> {
    let mut all: Vec<Client> = CLIENTS.lock().iter().flatten().copied().collect();
    all.sort_by_key(|c| c.connected_ms);
    all
}
