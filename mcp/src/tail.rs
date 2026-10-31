// SPDX-License-Identifier: GPL-3.0-only
//! Reading a log file on the SD card as it grows: its last lines, then whatever is added, by asking the
//! daemon for the file's size every half second and reading the new bytes.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::client::Client;

/// How often a followed file is checked.
pub const POLL: Duration = Duration::from_millis(500);
/// How far back from the end the last lines are looked for.
const WINDOW: u64 = 64 * 1024;
/// Most bytes read in one go.
const CHUNK: u64 = 1024 * 1024;

/// The last `lines` lines of `path`, and the offset following should continue from (its size).
pub async fn last_lines(client: &Client, path: &str, lines: usize) -> Result<(String, u64)> {
    let size = client.stat(path).await?.size;
    let start = size.saturating_sub(WINDOW);
    let (data, _) = client.read(path, start, size - start).await?;
    let text = String::from_utf8_lossy(&data);
    // A window that doesn't start at the beginning starts mid-line.
    let text = if start > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    let all: Vec<&str> = text.lines().collect();
    let mut out = all[all.len().saturating_sub(lines)..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok((out, start + data.len() as u64))
}

/// What follows `from` in `path` now: the new bytes, the next offset, and whether the file shrank (it was
/// truncated or replaced, so reading started again from its beginning).
pub async fn since(client: &Client, path: &str, from: u64) -> Result<(Vec<u8>, u64, bool)> {
    let size = client.stat(path).await?.size;
    let (from, restarted) = if size < from { (0, true) } else { (from, false) };
    if size == from {
        return Ok((Vec::new(), from, restarted));
    }
    let (data, _) = client.read(path, from, (size - from).min(CHUNK)).await?;
    let next = from + data.len() as u64;
    Ok((data, next, restarted))
}

/// What following found.
pub struct Followed {
    pub text: String,
    pub next: u64,
    /// `until` was seen.
    pub seen: bool,
    /// Older output dropped to stay under the limit.
    pub dropped: bool,
    pub restarted: bool,
}

/// Collects what's added to `path` after `from` for up to `length`, stopping early once a line contains
/// `until`. Keeps the newest `limit` bytes.
pub async fn follow(client: &Client, path: &str, from: u64, length: Duration, until: Option<&str>, limit: usize) -> Result<Followed> {
    let end = Instant::now() + length;
    let mut found = Followed { text: String::new(), next: from, seen: false, dropped: false, restarted: false };
    // The part of a line that hasn't ended yet, so `until` is matched against whole lines.
    let mut partial = String::new();
    while Instant::now() < end {
        tokio::time::sleep(POLL.min(end.saturating_duration_since(Instant::now()))).await;
        let (data, next, restarted) = since(client, path, found.next).await?;
        found.next = next;
        found.restarted |= restarted;
        if data.is_empty() {
            continue;
        }
        let new = String::from_utf8_lossy(&data);
        found.text.push_str(&new);
        if found.text.len() > limit {
            let mut cut = found.text.len() - limit;
            while !found.text.is_char_boundary(cut) {
                cut += 1;
            }
            found.text.drain(..cut);
            found.dropped = true;
        }
        if let Some(until) = until {
            partial.push_str(&new);
            let complete = partial.rfind('\n').map_or(0, |i| i + 1);
            if partial[..complete].lines().any(|line| line.contains(until)) {
                found.seen = true;
                break;
            }
            partial.drain(..complete);
        }
    }
    Ok(found)
}
