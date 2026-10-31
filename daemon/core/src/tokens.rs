// SPDX-License-Identifier: GPL-3.0-only
//! The tokens clients may present: those in [`TOKEN_PATH`] (one per paired PC, see [`proto::tokens`]) and
//! one passed on the command line (the NRO's `--token`). The file is read again at each connection, so a
//! line taken out of it (by hand, or by [`proto::Request::Unpair`]) shuts that PC out straight away.

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

/// The command line's token.
static EXTRA: nx::sync::Mutex<Option<String>> = nx::sync::Mutex::new(None);
/// The file's tokens when it was last read, kept for when it can't be read.
static FILE: nx::sync::Mutex<Vec<String>> = nx::sync::Mutex::new(Vec::new());

fn read_file() -> Result<String, Error> {
    match sd::read(TOKEN_PATH, 0, MAX_FILE + 1) {
        Ok((_, size)) if size as usize > MAX_FILE => Err(error(format!("{TOKEN_PATH} is over {MAX_FILE} bytes"))),
        Ok((data, _)) => String::from_utf8(data).map_err(|_| error(format!("{TOKEN_PATH} isn't UTF-8"))),
        Err(e) if e.kind == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

fn write_file(text: &str) -> Result<(), Error> {
    sd::create_dirs("/kickstand")?;
    let mut file = sd::create_for_write(STAGED, text.len() as u64)?;
    if !text.is_empty() {
        file.write_array::<u8, true>(text.as_bytes()).map_err(|rc| sd::io_error("write", STAGED, rc))?;
    }
    drop(file);
    sd::remove_if_exists(TOKEN_PATH)?;
    sd::rename(STAGED, TOKEN_PATH)
}

/// Reads [`TOKEN_PATH`] again (keeping what was read last time if it can't be), and returns every token
/// a client may present.
fn known() -> Vec<String> {
    match read_file() {
        Ok(text) => *FILE.lock() = proto::tokens::parse(&text).into_iter().map(String::from).collect(),
        Err(e) => log!("reading the tokens: {}", e.message),
    }
    let mut tokens = FILE.lock().clone();
    tokens.extend(EXTRA.lock().clone());
    tokens
}

/// Takes `extra` (a token given on the command line) and reads [`TOKEN_PATH`]. Returns how many there are.
pub fn load(extra: Option<&str>) -> usize {
    *EXTRA.lock() = extra.map(String::from);
    known().len()
}

/// Whether any client could connect, as of the last connection (without reading the file).
pub fn any() -> bool {
    !FILE.lock().is_empty() || EXTRA.lock().is_some()
}

/// Whether `token` may connect: `Err(())` if no token may.
pub fn check(token: &str) -> Result<bool, ()> {
    let known = known();
    if known.is_empty() {
        return Err(());
    }
    // Check every one, so the time taken doesn't say which matched.
    Ok(known.iter().fold(false, |found, known| proto::tokens::same(known, token) | found))
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
        write_file(&updated)?;
    }
    let mut tokens = FILE.lock();
    if !tokens.iter().any(|t| t == token) {
        tokens.push(String::from(token));
    }
    Ok(())
}

/// Takes `token` (and the comment pairing put above it) out of [`TOKEN_PATH`], so it can't connect again.
pub fn remove(token: &str) -> Result<(), Error> {
    let text = read_file()?;
    match proto::tokens::remove(&text, token) {
        Some(updated) => write_file(&updated)?,
        None if EXTRA.lock().as_deref().is_some_and(|extra| proto::tokens::same(extra, token)) => {
            let message = "this token was given to Kickstand on its command line (--token), not paired: remove it there";
            return Err(Error { kind: ErrorKind::Forbidden, message: message.into(), result: None });
        }
        None => return Err(Error { kind: ErrorKind::NotFound, message: format!("this PC's token isn't in {TOKEN_PATH}"), result: None }),
    }
    FILE.lock().retain(|t| t != token);
    Ok(())
}

fn error(message: String) -> Error {
    Error { kind: ErrorKind::Io, message, result: None }
}
