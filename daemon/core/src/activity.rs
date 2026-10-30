// SPDX-License-Identifier: GPL-3.0-only
//! Who is connected and what they're doing, in plain words, for the NRO's screen.

use alloc::vec::Vec;
use core::net::Ipv4Addr;

use proto::Request;

use crate::server::MAX_CLIENTS;

#[derive(Clone, Copy)]
pub struct Client {
    pub ip: Ipv4Addr,
    pub connected_ms: u64,
    pub requests: u32,
    /// The last request, as something a person would understand.
    pub doing: &'static str,
    pub last_ms: u64,
}

static CLIENTS: nx::sync::Mutex<[Option<Client>; MAX_CLIENTS]> = nx::sync::Mutex::new([None; MAX_CLIENTS]);

pub fn connected(slot: usize, ip: Ipv4Addr) {
    let now = crate::uptime_ms();
    CLIENTS.lock()[slot] = Some(Client { ip, connected_ms: now, requests: 0, doing: "connecting", last_ms: now });
}

pub fn disconnected(slot: usize) {
    CLIENTS.lock()[slot] = None;
}

pub fn request(slot: usize, req: &Request) {
    if let Some(client) = CLIENTS.lock()[slot].as_mut() {
        client.requests += 1;
        client.doing = describe(req);
        client.last_ms = crate::uptime_ms();
    }
}

/// The connected clients, longest connected first.
pub fn clients() -> Vec<Client> {
    let mut all: Vec<Client> = CLIENTS.lock().iter().flatten().copied().collect();
    all.sort_by_key(|c| c.connected_ms);
    all
}

fn describe(req: &Request) -> &'static str {
    match req {
        Request::Hello { .. } => "connecting",
        Request::Ping { .. } | Request::Status | Request::AppStatus | Request::AppletStatus | Request::FatalReports => {
            "checking on the console"
        }
        Request::ListDir { .. } | Request::Stat { .. } | Request::ReadFile { .. } => "reading files",
        Request::UploadBegin { .. } | Request::UploadChunk { .. } | Request::UploadFinish { .. } | Request::UploadAbort => {
            "copying a file to the SD card"
        }
        Request::Delete { .. } => "deleting a file",
        Request::SysmoduleStatus
        | Request::SysmoduleStart
        | Request::SysmoduleStop
        | Request::Handover => "managing the background service",
        Request::Exit => "closing Kickstand",
        Request::RebootToHekate { .. } => "restarting the console",
        Request::Press { .. } | Request::Input { .. } => "pressing buttons",
        Request::KeepAwake { .. } => "keeping the screen on",
        Request::IndicatorPosition { .. } => "placing the indicator",
        Request::Screenshot | Request::ScreenshotRead { .. } => "looking at the screen",
        Request::ListApps { .. } => "looking at installed software",
        Request::CloseApp | Request::CloseApplet => "closing software",
        Request::HblOverride { .. } => "opening homebrew",
        Request::Resources => "checking memory use",
        Request::IramPeek { .. }
        | Request::MemoryMap
        | Request::ReadMemory { .. }
        | Request::WriteMemory { .. }
        | Request::MemoryTarget { .. }
        | Request::Threads => "debugging the running software",
    }
}
