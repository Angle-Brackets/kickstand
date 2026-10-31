// SPDX-License-Identifier: GPL-3.0-only
//! Which SD card folders the agent may use without asking the user. The console only refuses what's
//! never safe (`proto::path`); everything else it allows, so this PC decides what needs a yes:
//!
//! - always: Kickstand's own folders ([`proto::path::OWN_DIRS`]), and reading crash reports;
//! - the folders `projects.json` deploys to (the user set those up);
//! - folders the user allowed: until the MCP server exits, or from now on (`~/.config/kickstand/
//!   access.json`, which `kickstand allow` edits).
//!
//! For anything else the MCP server asks the user through the AI app (an MCP elicitation: a question
//! in the app, whatever its permission mode), and the CLI goes ahead only when a person typed the
//! command (stdin is a terminal). This keeps an agent from wandering the card by mistake; it isn't a
//! wall against one set on getting around it, which could edit access.json like any file of the user's.

use std::sync::Mutex;

use anyhow::{Context, Result};
use proto::path::{OWN_DIRS, parent, under};
use serde::{Deserialize, Serialize};

/// What the agent wants to do with a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Need {
    Read,
    /// Write or delete; allowing it allows reading too.
    Write,
}

impl Need {
    pub fn verb(self) -> &'static str {
        match self {
            Need::Read => "read",
            Need::Write => "change",
        }
    }
}

/// Folders read without asking besides Kickstand's own: what the crash tools read.
const READ_FREELY: &[&str] = &["/atmosphere/crash_reports", "/atmosphere/fatal_errors"];

/// Allowed folders (normalized, no trailing slash).
#[derive(Default, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Grants {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

impl Grants {
    fn covers(&self, path: &str, need: Need) -> bool {
        let inside = |folders: &[String]| folders.iter().any(|f| f == "/" || under(path, f));
        inside(&self.write) || (need == Need::Read && inside(&self.read))
    }

    fn add(&mut self, folder: &str, need: Need) {
        let list = if need == Need::Write { &mut self.write } else { &mut self.read };
        if !list.iter().any(|f| f.eq_ignore_ascii_case(folder)) {
            list.push(folder.to_string());
        }
    }

    /// Removes `folder` from both lists; whether it was there.
    fn remove(&mut self, folder: &str) -> bool {
        let before = self.read.len() + self.write.len();
        self.read.retain(|f| !f.eq_ignore_ascii_case(folder));
        self.write.retain(|f| !f.eq_ignore_ascii_case(folder));
        before != self.read.len() + self.write.len()
    }
}

/// The allowances of one MCP server (one AI app session).
#[derive(Default)]
pub struct Access {
    session: Mutex<Grants>,
}

impl Access {
    /// Whether `path` (normalized) may be used for `need` without asking.
    pub fn allowed(&self, path: &str, need: Need) -> bool {
        builtin(path, need) || self.session.lock().unwrap().covers(path, need) || saved().covers(path, need)
    }

    /// Allows `folder` until this server exits.
    pub fn allow_for_session(&self, folder: &str, need: Need) {
        self.session.lock().unwrap().add(folder, need);
    }
}

/// Kickstand's own folders, crash reports, and where projects deploy to.
fn builtin(path: &str, need: Need) -> bool {
    let own = OWN_DIRS.iter().any(|dir| under(path, dir.trim_end_matches('/')));
    let read_freely = need == Need::Read && READ_FREELY.iter().any(|dir| under(path, dir));
    own || read_freely || project_folders().iter().any(|dir| under(path, dir))
}

/// The folders `projects.json` deploys NROs to (their data lives beside them).
fn project_folders() -> Vec<String> {
    let projects = crate::project::load_all().unwrap_or_default();
    projects.values().filter_map(|p| proto::path::normalize(&p.dest).ok()).map(|dest| parent(&dest).to_string()).filter(|dir| dir != "/").collect()
}

/// What to offer allowing for `path`: the folder itself when it's one (`is_dir`), else the folder it's in.
pub fn folder_of(path: &str, is_dir: bool) -> String {
    if is_dir { path.to_string() } else { parent(path).to_string() }
}

/// How `folder` reads in a question.
pub fn describe(folder: &str) -> String {
    if folder == "/" { "the whole SD card".into() } else { format!("{folder}/") }
}

fn file() -> Result<std::path::PathBuf> {
    Ok(crate::config::config_dir()?.join("access.json"))
}

/// The folders allowed from now on; none if the file is missing or unreadable.
pub fn saved() -> Grants {
    file().ok().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Allows `folder` (normalized) from now on.
pub fn save(folder: &str, need: Need) -> Result<()> {
    let mut grants = saved();
    grants.add(folder, need);
    write(&grants)
}

/// Stops allowing `folder` from now on; whether it was allowed.
pub fn forget(folder: &str) -> Result<bool> {
    let mut grants = saved();
    let removed = grants.remove(folder);
    write(&grants)?;
    Ok(removed)
}

fn write(grants: &Grants) -> Result<()> {
    let path = file()?;
    std::fs::create_dir_all(path.parent().context("config dir")?)?;
    std::fs::write(&path, serde_json::to_string_pretty(grants)? + "\n").with_context(|| format!("writing {}", path.display()))
}

/// The user's answer to "may the agent use this?", as the AI app shows it.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct Approval {
    /// For how long
    pub allow: AllowFor,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(inline)]
pub enum AllowFor {
    #[serde(rename = "Just this once")]
    Once,
    #[serde(rename = "This folder, until the app restarts")]
    Session,
    #[serde(rename = "This folder, from now on")]
    Always,
}

rmcp::elicit_safe!(Approval);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_cover_folders_and_write_covers_read() {
        let mut g = Grants::default();
        g.add("/config/sys-patch", Need::Read);
        g.add("/switch/mygame", Need::Write);
        assert!(g.covers("/config/sys-patch/config.ini", Need::Read));
        assert!(!g.covers("/config/sys-patch/config.ini", Need::Write));
        assert!(!g.covers("/config/sys-patch2/x", Need::Read));
        assert!(g.covers("/Switch/MyGame/save/1.bin", Need::Write));
        assert!(g.covers("/switch/mygame", Need::Read));
        assert!(!g.covers("/switch", Need::Read));
        assert!(g.remove("/config/sys-patch") && !g.covers("/config/sys-patch/x", Need::Read));
        g.add("/", Need::Read);
        assert!(g.covers("/anything", Need::Read));
    }

    #[test]
    fn kickstand_and_crash_reports_need_no_question() {
        assert!(builtin("/switch/kickstand/kickstand.nro", Need::Write));
        assert!(builtin("/kickstand/token.txt", Need::Read));
        assert!(builtin("/atmosphere/crash_reports/01.log", Need::Read));
        assert!(!builtin("/atmosphere/crash_reports/01.log", Need::Write));
        assert!(!builtin("/switch/kickstand2/x", Need::Read));
        assert_eq!(folder_of("/switch/mygame/mygame.nro", false), "/switch/mygame");
        assert_eq!(describe("/"), "the whole SD card");
    }
}
