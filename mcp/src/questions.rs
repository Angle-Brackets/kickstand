// SPDX-License-Identifier: GPL-3.0-only
//! Kickstand asking the user itself, for AI apps that can't show an MCP question (elicitation): the MCP
//! server writes the question to `~/.config/kickstand/questions/`, the desktop app shows it (it's opened
//! if it isn't running) and writes the answer beside it.
//!
//! Like `access.json`, these are files of the user's, so this keeps an agent from going ahead by mistake,
//! not one set on getting around it.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// How long an open desktop app's last sign of life counts.
const ALIVE: Duration = Duration::from_secs(8);
/// How long a just-opened desktop app gets to show up.
const OPENING: Duration = Duration::from_secs(20);
/// Questions older than this are left over from a server that went away.
const STALE: Duration = Duration::from_secs(600);
const HEARTBEAT: &str = ".desktop-open";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Question {
    pub id: String,
    /// The AI app the agent works in, if it said.
    pub app: Option<String>,
    /// What the agent wants to do ("put tico.nro at /switch/tico.nro and run it").
    pub action: String,
    /// What allowing the folder covers ("everything in /switch (reading, writing and deleting)").
    pub covers: String,
    /// Seconds since 1970, when asked.
    pub asked: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Once,
    Session,
    Always,
    No,
}

fn dir() -> Result<PathBuf> {
    Ok(crate::config::config_dir()?.join("questions"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The desktop app says it's open (it calls this every couple of seconds).
pub fn heartbeat() {
    if let Ok(dir) = dir() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(HEARTBEAT), now().to_string());
    }
}

fn desktop_open() -> bool {
    let Ok(dir) = dir() else { return false };
    let Ok(text) = std::fs::read_to_string(dir.join(HEARTBEAT)) else { return false };
    text.trim().parse::<u64>().is_ok_and(|at| now().saturating_sub(at) <= ALIVE.as_secs())
}

/// Questions waiting for an answer, oldest first.
pub fn pending() -> Vec<Question> {
    let Ok(dir) = dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut questions: Vec<Question> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str::<Question>(&std::fs::read_to_string(e.path()).ok()?).ok())
        .filter(|q| now().saturating_sub(q.asked) < STALE.as_secs() && !dir.join(format!("{}.answer", q.id)).exists())
        .collect();
    questions.sort_by_key(|q| q.asked);
    questions
}

/// The desktop app's answer to question `id`.
pub fn answer(id: &str, answer: Answer) -> Result<()> {
    let path = dir()?.join(format!("{id}.answer"));
    std::fs::write(&path, serde_json::to_string(&answer)?).with_context(|| format!("writing {}", path.display()))
}

/// Asks the user in the desktop app, opening it if needed, and waits up to `timeout` for the answer.
/// `Ok(None)`: no answer in time. `Err`: the question couldn't be shown (no desktop app here).
pub async fn ask(app: Option<String>, action: &str, covers: &str, timeout: Duration) -> Result<Option<Answer>> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let id = format!("{}-{}-{}", now(), std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let dir = dir()?;
    std::fs::create_dir_all(&dir)?;
    let question = Question { id: id.clone(), app, action: action.into(), covers: covers.into(), asked: now() };
    let asked = dir.join(format!("{id}.json"));
    let answered = dir.join(format!("{id}.answer"));
    std::fs::write(&asked, serde_json::to_string_pretty(&question)?)?;
    let result = wait(&answered, timeout).await;
    let _ = std::fs::remove_file(&asked);
    let _ = std::fs::remove_file(&answered);
    result
}

async fn wait(answered: &std::path::Path, timeout: Duration) -> Result<Option<Answer>> {
    let start = tokio::time::Instant::now();
    if !desktop_open() {
        let exe = std::env::current_exe()?;
        std::process::Command::new(exe)
            .arg("desktop")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("couldn't open Kickstand's window to ask: {e}"))?;
    }
    loop {
        if let Ok(text) = std::fs::read_to_string(answered) {
            return Ok(Some(serde_json::from_str(&text).context("reading the answer")?));
        }
        let waited = start.elapsed();
        if waited >= OPENING && !desktop_open() {
            return Err(anyhow!("Kickstand's window didn't open to ask (no display here?)"));
        }
        if waited >= timeout {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
