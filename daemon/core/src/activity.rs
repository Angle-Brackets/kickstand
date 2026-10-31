// SPDX-License-Identifier: GPL-3.0-only
//! Who is connected and what they're doing, in plain words, for the NRO's screen and
//! [`Request::Activity`] and [`Request::Agents`].

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU32, Ordering};

use proto::{MAX_RECENT, ROUTINE, Request};

use crate::server::MAX_CLIENTS;

#[derive(Clone)]
pub struct Client {
    /// Numbers connections since the daemon started, from 1.
    pub id: u32,
    pub slot: usize,
    pub ip: Ipv4Addr,
    /// The AI app it works for ([`Request::Introduce`]).
    pub app: Option<String>,
    pub connected_ms: u64,
    pub requests: u32,
    /// The last request, as something a person would understand.
    pub doing: &'static str,
    pub last_ms: u64,
    /// The last request that changed something ([`proto::changes_console`]), and when.
    pub change: Option<(&'static str, u64)>,
}

/// A client turning to something else.
#[derive(Clone)]
pub struct Event {
    pub at_ms: u64,
    pub id: u32,
    pub ip: Ipv4Addr,
    pub app: Option<String>,
    pub what: &'static str,
}

static CLIENTS: nx::sync::Mutex<[Option<Client>; MAX_CLIENTS]> = nx::sync::Mutex::new([const { None }; MAX_CLIENTS]);
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
/// Oldest first.
static RECENT: nx::sync::Mutex<VecDeque<Event>> = nx::sync::Mutex::new(VecDeque::new());

pub fn connected(slot: usize, ip: Ipv4Addr) {
    let now = crate::uptime_ms();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    CLIENTS.lock()[slot] = Some(Client { id, slot, ip, app: None, connected_ms: now, requests: 0, doing: "connecting", last_ms: now, change: None });
}

/// Names the app the client in `slot` works for, cut to [`proto::MAX_APP_NAME`] bytes.
pub fn introduce(slot: usize, mut app: String) {
    let mut end = app.len().min(proto::MAX_APP_NAME);
    while !app.is_char_boundary(end) {
        end -= 1;
    }
    app.truncate(end);
    if let Some(client) = CLIENTS.lock()[slot].as_mut() {
        client.app = Some(app);
    }
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
            if proto::changes_console(doing) {
                client.change = Some((doing, now));
            }
            changed.then(|| (client.id, client.ip, client.app.clone()))
        }
        None => None,
    };
    if let Some((id, ip, app)) = changed
        && !ROUTINE.contains(&doing)
    {
        let mut recent = RECENT.lock();
        if recent.len() == MAX_RECENT {
            recent.pop_front();
        }
        recent.push_back(Event { at_ms: now, id, ip, app, what: doing });
    }
}

/// What clients did lately, newest first.
pub fn recent() -> Vec<Event> {
    RECENT.lock().iter().rev().cloned().collect()
}

/// The connected clients, longest connected first.
pub fn clients() -> Vec<Client> {
    let mut all: Vec<Client> = CLIENTS.lock().iter().flatten().cloned().collect();
    all.sort_by_key(|c| c.connected_ms);
    all
}
