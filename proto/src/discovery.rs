// SPDX-License-Identifier: GPL-3.0-only
//! Finding consoles on the local network, so nobody types an IP address. The PC broadcasts a [`Probe`] to
//! UDP [`PORT`]; every daemon that hears it answers the sender with an [`Announce`], and the console's
//! address is where the answer came from.
//!
//! A datagram is a 4-byte magic (which also tells the two apart) followed by postcard. Like [`crate::Request`]
//! they only grow: fields are appended, never changed, and readers ignore what follows the fields they
//! know (postcard leaves trailing bytes alone).

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::DaemonKind;

/// UDP port daemons listen on for probes (next to the TCP [`crate::DEFAULT_PORT`]).
pub const PORT: u16 = 28791;
/// Larger than any announce; longer datagrams are ignored.
pub const MAX_DATAGRAM: usize = 512;
/// Longest nickname an announce carries, in bytes.
pub const MAX_NICKNAME: usize = 64;

const PROBE_MAGIC: [u8; 4] = *b"KSp1";
const ANNOUNCE_MAGIC: [u8; 4] = *b"KSa1";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Echoed in the announce, so a PC can tell its answers from stale ones.
    pub nonce: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    pub nonce: u64,
    /// The daemon's [`crate::PROTOCOL_VERSION`].
    pub protocol: u32,
    pub daemon_version: String,
    pub kind: DaemonKind,
    /// The TCP port to connect to.
    pub port: u16,
    /// The name set in System Settings ("Ankit's Switch"), empty if unknown.
    pub nickname: String,
    /// The console is showing a pairing code ([`crate::Request::Pair`] is accepted).
    pub pairing: bool,
}

pub fn encode_probe(probe: &Probe) -> Vec<u8> {
    with_magic(PROBE_MAGIC, probe)
}

pub fn decode_probe(datagram: &[u8]) -> Option<Probe> {
    postcard::from_bytes(datagram.strip_prefix(&PROBE_MAGIC)?).ok()
}

pub fn encode_announce(announce: &Announce) -> Vec<u8> {
    with_magic(ANNOUNCE_MAGIC, announce)
}

pub fn decode_announce(datagram: &[u8]) -> Option<Announce> {
    postcard::from_bytes(datagram.strip_prefix(&ANNOUNCE_MAGIC)?).ok()
}

fn with_magic<T: Serialize>(magic: [u8; 4], msg: &T) -> Vec<u8> {
    // Only allocation can fail, and that aborts.
    postcard::to_extend(msg, Vec::from(magic)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_ignores_the_other_kind() {
        let probe = Probe { nonce: 7 };
        let bytes = encode_probe(&probe);
        assert_eq!(decode_probe(&bytes), Some(probe));
        assert_eq!(decode_announce(&bytes), None);

        let announce = Announce {
            nonce: 7,
            protocol: 21,
            daemon_version: "0.1.0".into(),
            kind: DaemonKind::Nro,
            port: 28790,
            nickname: "Switch".into(),
            pairing: true,
        };
        let mut bytes = encode_announce(&announce);
        assert_eq!(decode_announce(&bytes), Some(announce.clone()));
        assert_eq!(decode_probe(&bytes), None);
        // A newer daemon's extra fields are skipped.
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(decode_announce(&bytes), Some(announce));
        assert_eq!(decode_announce(b"KSa1"), None);
    }
}
