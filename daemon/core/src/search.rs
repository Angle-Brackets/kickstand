// SPDX-License-Identifier: GPL-3.0-only
//! Searching the SD card on the console, so a folder tree or a big log doesn't cross Wi-Fi to be
//! filtered: [`find`] by name, [`grep`] for a string. Both skip what the daemon won't read
//! ([`proto::path::check_read`]) and stop after [`TIME_LIMIT_MS`] with what they have, under the
//! client's reply timeout.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use proto::{DirEntry, Error, GrepMatch, MAX_CHUNK, MAX_DIR_ENTRIES, MAX_GREP_FILE, MAX_GREP_LINE, path};

use crate::{sd, uptime_ms};

const TIME_LIMIT_MS: u64 = 20_000;
/// The start of a file checked for NUL bytes: a file with one is binary, and grep skips it.
const BINARY_SNIFF: usize = 1024;
/// A longer line is cut, so one huge line can't use up the heap.
const MAX_LINE_BYTES: usize = 4096;

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// What a walk found, and whether it stopped early (too many results, or out of time).
pub struct Found<T> {
    pub items: Vec<T>,
    pub truncated: bool,
}

/// Entries under `dir` whose name contains `name` (ASCII case ignored), as full paths, at most `max`.
pub fn find(dir: &str, name: &str, max: usize) -> Result<Found<DirEntry>, Error> {
    let deadline = uptime_ms() + TIME_LIMIT_MS;
    let needle = name.to_ascii_lowercase();
    let mut found = Found { items: Vec::new(), truncated: false };
    let mut stack = Vec::from([String::from(dir)]);
    let mut first = true;
    while let Some(dir) = stack.pop() {
        if uptime_ms() > deadline {
            found.truncated = true;
            break;
        }
        let (entries, cut) = match sd::list_dir(&dir, MAX_DIR_ENTRIES) {
            Ok(listing) => listing,
            // The starting folder must exist; one that vanished mid-walk is skipped.
            Err(e) if first => return Err(e),
            Err(_) => continue,
        };
        first = false;
        found.truncated |= cut;
        for entry in entries {
            let full = join(&dir, &entry.name);
            if path::check_read(&full).is_err() {
                continue;
            }
            let is_dir = entry.is_dir;
            if entry.name.to_ascii_lowercase().contains(&needle) {
                if found.items.len() == max {
                    found.truncated = true;
                    return Ok(found);
                }
                found.items.push(DirEntry { name: full.clone(), ..entry });
            }
            if is_dir {
                stack.push(full);
            }
        }
    }
    Ok(found)
}

/// Lines containing `needle` in the file `target`, or in the files under it if it's a folder, at most
/// `max`; also how many files were searched.
pub fn grep(target: &str, needle: &str, ignore_case: bool, max: usize) -> Result<(Found<GrepMatch>, u32), Error> {
    let deadline = uptime_ms() + TIME_LIMIT_MS;
    let needle: Vec<u8> = if ignore_case { needle.to_ascii_lowercase().into_bytes() } else { needle.as_bytes().to_vec() };
    let mut found = Found { items: Vec::new(), truncated: false };
    let mut files = 0;
    let top = sd::stat(target)?;
    let mut paths = Vec::new();
    if top.is_dir {
        let all = find(target, "", usize::MAX)?;
        found.truncated |= all.truncated;
        paths.extend(all.items.into_iter().filter(|e| !e.is_dir && e.size <= MAX_GREP_FILE).map(|e| e.name));
    } else {
        paths.push(String::from(target));
    }
    for file in paths {
        if uptime_ms() > deadline || found.items.len() >= max {
            found.truncated = true;
            break;
        }
        files += 1;
        // Under a folder, a file that can't be read is passed over.
        if let Err(e) = grep_file(&file, &needle, ignore_case, max, deadline, &mut found)
            && !top.is_dir
        {
            return Err(e);
        }
    }
    Ok((found, files))
}

fn grep_file(file: &str, needle: &[u8], ignore_case: bool, max: usize, deadline: u64, found: &mut Found<GrepMatch>) -> Result<(), Error> {
    let mut offset = 0u64;
    let mut line_no = 1u32;
    let mut line: Vec<u8> = Vec::new();
    loop {
        let (data, size) = sd::read(file, offset, MAX_CHUNK)?;
        if offset == 0 && data[..data.len().min(BINARY_SNIFF)].contains(&0) {
            return Ok(());
        }
        offset += data.len() as u64;
        let at_end = data.is_empty() || offset >= size;
        let mut rest = &data[..];
        while !rest.is_empty() {
            let (part, ended) = match rest.iter().position(|&b| b == b'\n') {
                Some(i) => (&rest[..i], true),
                None => (rest, false),
            };
            let room = MAX_LINE_BYTES.saturating_sub(line.len());
            line.extend_from_slice(&part[..part.len().min(room)]);
            rest = if ended { &rest[part.len() + 1..] } else { &[] };
            if ended {
                if check_line(file, line_no, &line, needle, ignore_case, max, found) {
                    return Ok(());
                }
                line.clear();
                line_no += 1;
            }
        }
        if at_end {
            if !line.is_empty() {
                check_line(file, line_no, &line, needle, ignore_case, max, found);
            }
            return Ok(());
        }
        if uptime_ms() > deadline {
            found.truncated = true;
            return Ok(());
        }
    }
}

/// Adds `line` if it matches; true once `max` matches are in.
fn check_line(file: &str, line_no: u32, line: &[u8], needle: &[u8], ignore_case: bool, max: usize, found: &mut Found<GrepMatch>) -> bool {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let matches = if ignore_case {
        line.windows(needle.len().max(1)).any(|w| w.eq_ignore_ascii_case(needle))
    } else {
        line.windows(needle.len().max(1)).any(|w| w == needle)
    };
    if !matches {
        return false;
    }
    if found.items.len() == max {
        found.truncated = true;
        return true;
    }
    let mut text = String::from_utf8_lossy(line).into_owned();
    if text.len() > MAX_GREP_LINE {
        let mut cut = MAX_GREP_LINE;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    found.items.push(GrepMatch { path: String::from(file), line: line_no, text });
    found.items.len() == max
}
