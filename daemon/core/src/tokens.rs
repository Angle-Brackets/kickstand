// SPDX-License-Identifier: GPL-3.0-only
//! The tokens clients may present: those in [`TOKEN_PATH`] (one per paired PC, see [`proto::tokens`]) and
//! one passed on the command line (the NRO's `--token`). Pairing adds to both the file and the list.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use proto::path::TOKEN_PATH;
use proto::tokens::AddError;
use proto::{Error, ErrorKind};

use crate::{log, sd};

/// Larger than a file of [`proto::MAX_TOKENS`] tokens with their comments.
const MAX_FILE: usize = 16 * 1024;
/// Written here first, then moved over [`TOKEN_PATH`].
const STAGED: &str = "/kickstand/token.txt.kickstand";

static TOKENS: nx::sync::Mutex<Vec<String>> = nx::sync::Mutex::new(Vec::new());

fn read_file() -> Result<String, Error> {
    match sd::read(TOKEN_PATH, 0, MAX_FILE + 1) {
        Ok((_, size)) if size as usize > MAX_FILE => Err(error(format!("{TOKEN_PATH} is over {MAX_FILE} bytes"))),
        Ok((data, _)) => String::from_utf8(data).map_err(|_| error(format!("{TOKEN_PATH} isn't UTF-8"))),
        Err(e) if e.kind == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Reads [`TOKEN_PATH`], plus `extra` (a token given on the command line). Returns how many there are.
pub fn load(extra: Option<&str>) -> usize {
    let text = read_file().unwrap_or_else(|e| {
        log!("reading the tokens: {}", e.message);
        String::new()
    });
    let mut tokens: Vec<String> = proto::tokens::parse(&text).into_iter().map(String::from).collect();
    tokens.extend(extra.map(String::from));
    let count = tokens.len();
    *TOKENS.lock() = tokens;
    count
}

/// Whether any client could connect.
pub fn any() -> bool {
    !TOKENS.lock().is_empty()
}

pub fn accepts(token: &str) -> bool {
    // Check every one, so the time taken doesn't say which matched.
    TOKENS.lock().iter().fold(false, |found, known| proto::tokens::same(known, token) | found)
}

/// Saves `token` as a paired PC's (`who`, its address, goes in a comment) and accepts it from now on.
pub fn add_paired(token: &str, who: &str) -> Result<(), Error> {
    let text = read_file()?;
    let updated = match proto::tokens::add(&text, token, who) {
        Ok(updated) => updated,
        Err(AddError::Invalid) => return Err(Error { kind: ErrorKind::BadRequest, message: "that token can't be stored".into(), result: None }),
        Err(AddError::Full) => {
            let message = format!("{TOKEN_PATH} already holds {} tokens: remove old ones first", proto::MAX_TOKENS);
            return Err(Error { kind: ErrorKind::Forbidden, message, result: None });
        }
    };
    if let Some(updated) = updated {
        sd::create_dirs("/kickstand")?;
        let mut file = sd::create_for_write(STAGED, updated.len() as u64)?;
        file.write_array::<u8, true>(updated.as_bytes()).map_err(|rc| sd::io_error("write", STAGED, rc))?;
        drop(file);
        sd::remove_if_exists(TOKEN_PATH)?;
        sd::rename(STAGED, TOKEN_PATH)?;
    }
    let mut tokens = TOKENS.lock();
    if !tokens.iter().any(|t| t == token) {
        tokens.push(String::from(token));
    }
    Ok(())
}

fn error(message: String) -> Error {
    Error { kind: ErrorKind::Io, message, result: None }
}
