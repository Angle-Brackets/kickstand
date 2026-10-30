// SPDX-License-Identifier: GPL-3.0-only
//! Adding Kickstand to the MCP clients people use, so nobody edits a config by hand:
//! - Claude Code: `claude mcp add -s user kickstand -- <this program> serve`; whether it's there is read
//!   from `~/.claude.json` (user scope `mcpServers`, or any project's), which `claude mcp get` would only
//!   tell after starting every server to check it.
//! - Claude Desktop: the `kickstand` entry of `mcpServers` in `claude_desktop_config.json`, every other key
//!   kept. The file as it was before Kickstand first changed it is kept beside it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// The server's name in the clients' configs.
pub const NAME: &str = "kickstand";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum App {
    ClaudeCode,
    ClaudeDesktop,
}

impl App {
    pub const ALL: [App; 2] = [App::ClaudeCode, App::ClaudeDesktop];

    pub fn name(self) -> &'static str {
        match self {
            App::ClaudeCode => "Claude Code",
            App::ClaudeDesktop => "Claude Desktop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The app isn't installed (no `claude` command, no Claude Desktop config folder).
    Missing,
    NotLinked,
    /// Linked, running this command.
    Linked { command: String },
}

/// What runs Kickstand's MCP server: this program, with `serve`.
pub fn server_command() -> Result<(String, Vec<String>)> {
    let exe = std::env::current_exe().context("finding this program's path")?;
    Ok((exe.to_string_lossy().into_owned(), vec!["serve".into()]))
}

pub fn state(app: App) -> State {
    match app {
        App::ClaudeCode => {
            if find_on_path("claude").is_none() {
                return State::Missing;
            }
            let config = home().map(|h| h.join(".claude.json"));
            let text = config.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
            claude_code_entry(&text).map_or(State::NotLinked, |command| State::Linked { command })
        }
        App::ClaudeDesktop => {
            let Some(path) = desktop_config_path() else { return State::Missing };
            if !path.parent().is_some_and(Path::exists) {
                return State::Missing;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            desktop_entry(&text).map_or(State::NotLinked, |command| State::Linked { command })
        }
    }
}

/// Adds Kickstand to `app`; returns what to tell the user.
pub fn link(app: App) -> Result<String> {
    let (command, args) = server_command()?;
    match app {
        App::ClaudeCode => {
            let claude = find_on_path("claude").context("Claude Code's `claude` command isn't installed")?;
            if let State::Linked { .. } = state(app) {
                // Replaced, so it runs this copy of Kickstand.
                run(&claude, &["mcp", "remove", "-s", "user", NAME]).ok();
            }
            let mut add = vec!["mcp", "add", "-s", "user", NAME, "--", &command];
            add.extend(args.iter().map(String::as_str));
            run(&claude, &add)?;
            Ok("linked: start a new Claude Code session to use Kickstand".into())
        }
        App::ClaudeDesktop => {
            let path = desktop_config_path().context("no Claude Desktop config location on this system")?;
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let updated = with_desktop_entry(&text, &command, &args)?;
            write_desktop(&path, &text, &updated)?;
            Ok("linked: restart Claude Desktop to use Kickstand".into())
        }
    }
}

/// Takes Kickstand out of `app`'s config.
pub fn unlink(app: App) -> Result<String> {
    match app {
        App::ClaudeCode => {
            let claude = find_on_path("claude").context("Claude Code's `claude` command isn't installed")?;
            run(&claude, &["mcp", "remove", "-s", "user", NAME])?;
            Ok("unlinked from Claude Code".into())
        }
        App::ClaudeDesktop => {
            let path = desktop_config_path().context("no Claude Desktop config location on this system")?;
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            match without_desktop_entry(&text)? {
                Some(updated) => write_desktop(&path, &text, &updated)?,
                None => return Ok("Claude Desktop didn't have Kickstand".into()),
            }
            Ok("unlinked: restart Claude Desktop".into())
        }
    }
}

/// Kickstand's command in a `~/.claude.json`, user scope first, then any project's.
fn claude_code_entry(text: &str) -> Option<String> {
    let config: Value = serde_json::from_str(text).ok()?;
    let user = config.get("mcpServers").and_then(|s| s.get(NAME));
    let project = || config.get("projects")?.as_object()?.values().find_map(|p| p.get("mcpServers")?.get(NAME));
    user.or_else(project).map(describe)
}

fn desktop_entry(text: &str) -> Option<String> {
    let config: Value = serde_json::from_str(text).ok()?;
    config.get("mcpServers")?.get(NAME).map(describe)
}

/// `command args…` of a server entry.
fn describe(entry: &Value) -> String {
    let command = entry.get("command").and_then(Value::as_str).unwrap_or("?");
    let args = entry.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    std::iter::once(command).chain(args).collect::<Vec<_>>().join(" ")
}

/// `text` (a Claude Desktop config, possibly empty) with Kickstand's entry set.
fn with_desktop_entry(text: &str, command: &str, args: &[String]) -> Result<String> {
    let mut config = parse_object(text)?;
    let servers = config.entry("mcpServers").or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else { bail!("its mcpServers isn't an object; fix it by hand") };
    servers.insert(NAME.into(), json!({ "command": command, "args": args }));
    Ok(serde_json::to_string_pretty(&config)? + "\n")
}

/// `text` without Kickstand's entry, or `None` if it had none.
fn without_desktop_entry(text: &str) -> Result<Option<String>> {
    let mut config = parse_object(text)?;
    let removed = config.get_mut("mcpServers").and_then(Value::as_object_mut).and_then(|s| s.remove(NAME));
    match removed {
        Some(_) => Ok(Some(serde_json::to_string_pretty(&config)? + "\n")),
        None => Ok(None),
    }
}

fn parse_object(text: &str) -> Result<Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(text).context("the config isn't valid JSON; fix it by hand")? {
        Value::Object(map) => Ok(map),
        _ => bail!("the config isn't a JSON object; fix it by hand"),
    }
}

/// Writes through a temporary file, after saving the original once as `<name>.before-kickstand`.
fn write_desktop(path: &Path, old: &str, new: &str) -> Result<()> {
    let backup = path.with_extension("json.before-kickstand");
    if !old.is_empty() && !backup.exists() {
        std::fs::write(&backup, old).with_context(|| format!("backing up {}", path.display()))?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let staged = path.with_extension("json.kickstand");
    std::fs::write(&staged, new).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn desktop_config_path() -> Option<PathBuf> {
    let file = "Claude/claude_desktop_config.json";
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join(file))
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library/Application Support").join(file))
    } else {
        // No official Linux build; the community ones use this.
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".config")));
        config.map(|c| c.join(file))
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) { vec![format!("{name}.exe"), format!("{name}.cmd")] } else { vec![name.into()] };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file())
}

fn run(program: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new(program).args(args).output().with_context(|| format!("running {}", program.display()))?;
    if !out.status.success() {
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        bail!("`claude {}` failed: {}", args.join(" "), text.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_config_keeps_everything_else() {
        let config = r#"{"theme":"dark","mcpServers":{"other":{"command":"x"}}}"#;
        let linked = with_desktop_entry(config, "/opt/kickstand", &["serve".into()]).unwrap();
        let value: Value = serde_json::from_str(&linked).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(desktop_entry(&linked).as_deref(), Some("/opt/kickstand serve"));

        let unlinked = without_desktop_entry(&linked).unwrap().unwrap();
        assert_eq!(desktop_entry(&unlinked), None);
        assert_eq!(serde_json::from_str::<Value>(&unlinked).unwrap()["mcpServers"]["other"]["command"], "x");
        assert_eq!(without_desktop_entry(&unlinked).unwrap(), None);
    }

    #[test]
    fn desktop_config_from_nothing_and_bad_files() {
        let linked = with_desktop_entry("", "k", &["serve".into()]).unwrap();
        assert_eq!(desktop_entry(&linked).as_deref(), Some("k serve"));
        assert!(with_desktop_entry("{not json", "k", &[]).is_err());
        assert!(with_desktop_entry(r#"{"mcpServers":3}"#, "k", &[]).is_err());
    }

    #[test]
    fn finds_claude_code_entries_in_either_scope() {
        let user = r#"{"mcpServers":{"kickstand":{"type":"stdio","command":"/a/kickstand","args":["serve"]}}}"#;
        assert_eq!(claude_code_entry(user).as_deref(), Some("/a/kickstand serve"));
        let project = r#"{"projects":{"/p":{"mcpServers":{"kickstand":{"command":"/b/kickstand","args":["serve"]}}}}}"#;
        assert_eq!(claude_code_entry(project).as_deref(), Some("/b/kickstand serve"));
        assert_eq!(claude_code_entry(r#"{"projects":{"/p":{}}}"#), None);
    }
}
