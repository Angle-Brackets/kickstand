// SPDX-License-Identifier: GPL-3.0-only
//! The pairing code the on-console screen shows ([`proto::Request::Pair`]). Only the NRO starts it, as only
//! it can show the code; a code is replaced after [`proto::PAIR_MAX_TRIES`] wrong guesses, and each wrong
//! guess costs a second, so it can't be worked out over the network.

use alloc::string::String;

use nx::service;
use nx::service::spl::{IRandomClient, RandomService};
use nx::svc;
use proto::{Error, ErrorKind, PAIR_CODE_LEN, PAIR_MAX_TRIES};

use crate::log;

struct State {
    code: String,
    wrong: u32,
}

static STATE: nx::sync::Mutex<Option<State>> = nx::sync::Mutex::new(None);

const WRONG_DELAY_NS: i64 = 1_000_000_000;

/// Starts showing a code (a new one).
pub fn start() {
    let code = new_code();
    *STATE.lock() = code.map(|code| State { code, wrong: 0 });
}

/// The code to show, if pairing is on.
pub fn code() -> Option<String> {
    STATE.lock().as_ref().map(|s| s.code.clone())
}

/// Checks `code` and pairs `token` (from `who`) if it's right.
pub fn pair(code: &str, token: &str, who: &str) -> Result<(), Error> {
    {
        let mut state = STATE.lock();
        let Some(current) = state.as_mut() else {
            return Err(error(ErrorKind::Unsupported, "not pairing: open Kickstand from hbmenu on the Switch to see a code".into()));
        };
        if !proto::tokens::same(&current.code, code) {
            current.wrong += 1;
            let replaced = current.wrong >= PAIR_MAX_TRIES;
            drop(state);
            log!("wrong pairing code from {who}");
            if replaced {
                log!("{PAIR_MAX_TRIES} wrong pairing codes: showing a new one");
                start();
            }
            let _ = svc::sleep_thread(WRONG_DELAY_NS);
            return Err(error(ErrorKind::Unauthorized, "wrong pairing code".into()));
        }
    }
    crate::tokens::add_paired(token, who)?;
    log!("paired with the PC at {who}");
    // Shown again only for the next PC.
    start();
    Ok(())
}

/// `PAIR_CODE_LEN` random digits from csrng.
fn new_code() -> Option<String> {
    let mut bytes = [0u8; PAIR_CODE_LEN * 2];
    let filled = service::new_service_object::<RandomService>()
        .and_then(|srv| srv.generate_random_bytes(nx::ipc::sf::Buffer::from_mut_array(&mut bytes)));
    if let Err(rc) = filled {
        log!("no pairing code (csrng: {rc})");
        return None;
    }
    // Two bytes per digit keeps the modulo bias tiny (65536 % 10 = 6).
    Some(bytes.as_chunks::<2>().0.iter().map(|&pair| char::from(b'0' + (u16::from_le_bytes(pair) % 10) as u8)).collect())
}

fn error(kind: ErrorKind, message: String) -> Error {
    Error { kind, message, result: None }
}
