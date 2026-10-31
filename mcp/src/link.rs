// SPDX-License-Identifier: GPL-3.0-only
//! Adding Kickstand to the MCP clients people use, so nobody edits a config by hand:
//! - Claude Code: `claude mcp add -s user kickstand -- <this program> serve`; whether it's there is read
//!   from `~/.claude.json` (user scope `mcpServers`, or any project's), which `claude mcp get` would only
//!   tell after starting every server to check it.
//! - Claude Desktop, Gemini CLI, Antigravity and LM Studio: the `kickstand` entry of `mcpServers` in their
//!   JSON config, every other key kept.
//! - Codex (OpenAI's CLI, IDE extension and app share it): `[mcp_servers.kickstand]` in
//!   `~/.codex/config.toml`, edited with toml_edit so comments and layout survive. Written from Codex's
//!   documentation; not tried against Codex itself.
//!
//! The file as it was before Kickstand first changed it is kept beside it (`.before-kickstand`).

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
    GeminiCli,
    Antigravity,
    Codex,
    LmStudio,
}

/// Who makes an app: the desktop app's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maker {
    Claude,
    Gemini,
    OpenAi,
    /// Anything else that speaks MCP, such as LM Studio running a local model.
    Custom,
}

impl Maker {
    pub const ALL: [Maker; 4] = [Maker::Claude, Maker::Gemini, Maker::OpenAi, Maker::Custom];

    pub fn name(self) -> &'static str {
        match self {
            Maker::Claude => "Claude",
            Maker::Gemini => "Gemini",
            Maker::OpenAi => "OpenAI",
            Maker::Custom => "Custom",
        }
    }
}

/// How an app's config holds its MCP servers.
enum Config {
    /// Claude Code's own command.
    ClaudeCli,
    /// A JSON file with an `mcpServers` object.
    Json(PathBuf),
    /// Codex's TOML file with `[mcp_servers.<name>]` tables.
    CodexToml(PathBuf),
}

impl App {
    pub const ALL: [App; 6] = [App::ClaudeCode, App::ClaudeDesktop, App::GeminiCli, App::Antigravity, App::Codex, App::LmStudio];

    pub fn name(self) -> &'static str {
        match self {
            App::ClaudeCode => "Claude Code",
            App::ClaudeDesktop => "Claude Desktop",
            App::GeminiCli => "Gemini CLI",
            App::Antigravity => "Antigravity",
            App::Codex => "Codex",
            App::LmStudio => "LM Studio",
        }
    }

    pub fn maker(self) -> Maker {
        match self {
            App::ClaudeCode | App::ClaudeDesktop => Maker::Claude,
            App::GeminiCli | App::Antigravity => Maker::Gemini,
            App::Codex => Maker::OpenAi,
            App::LmStudio => Maker::Custom,
        }
    }

    /// A line under its name.
    pub fn detail(self) -> &'static str {
        match self {
            App::ClaudeCode => "The terminal agent",
            App::ClaudeDesktop => "The desktop app",
            App::GeminiCli => "The terminal agent",
            App::Antigravity => "Google's agent IDE",
            App::Codex => "CLI, IDE extension and app",
            App::LmStudio => "Local models, like DeepSeek or Qwen",
        }
    }

    /// The name `link` and `--remove` take on the command line.
    pub fn slug(self) -> &'static str {
        match self {
            App::ClaudeCode => "code",
            App::ClaudeDesktop => "desktop",
            App::GeminiCli => "gemini",
            App::Antigravity => "antigravity",
            App::Codex => "codex",
            App::LmStudio => "lmstudio",
        }
    }

    pub fn from_slug(slug: &str) -> Option<App> {
        let slug = slug.to_ascii_lowercase();
        App::ALL.into_iter().find(|app| app.slug() == slug || app.name().to_ascii_lowercase().replace(' ', "-") == slug)
    }

    /// What to do after linking or unlinking for it to take effect.
    fn restart(self) -> &'static str {
        match self {
            App::ClaudeCode | App::GeminiCli | App::Codex => "start a new session",
            App::ClaudeDesktop | App::Antigravity | App::LmStudio => "restart it",
        }
    }

    /// Where its config is, if this system has a place for it.
    fn config(self) -> Option<Config> {
        let home = home();
        Some(match self {
            App::ClaudeCode => Config::ClaudeCli,
            App::ClaudeDesktop => Config::Json(desktop_config_path()?),
            App::GeminiCli => Config::Json(home?.join(".gemini/settings.json")),
            App::Antigravity => Config::Json(home?.join(".gemini/antigravity/mcp_config.json")),
            App::Codex => Config::CodexToml(std::env::var_os("CODEX_HOME").map(PathBuf::from).or(home.map(|h| h.join(".codex")))?.join("config.toml")),
            App::LmStudio => Config::Json(home?.join(".lmstudio/mcp.json")),
        })
    }

    /// Whether it's installed: its command, or the folder it keeps its settings in.
    fn installed(self) -> bool {
        let folder = |path: Option<PathBuf>| path.is_some_and(|p| p.is_dir());
        let home = home();
        match self {
            App::ClaudeCode => find_on_path("claude").is_some(),
            App::ClaudeDesktop => folder(desktop_config_path().and_then(|p| p.parent().map(Path::to_path_buf))),
            // ~/.gemini alone could be Antigravity's.
            App::GeminiCli => find_on_path("gemini").is_some() || home.is_some_and(|h| h.join(".gemini/settings.json").is_file()),
            App::Antigravity => folder(home.map(|h| h.join(".gemini/antigravity"))),
            App::Codex => find_on_path("codex").is_some() || folder(home.map(|h| h.join(".codex"))),
            App::LmStudio => folder(home.map(|h| h.join(".lmstudio"))),
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
    if !app.installed() {
        return State::Missing;
    }
    let entry = match app.config() {
        None => None,
        Some(Config::ClaudeCli) => {
            let text = home().and_then(|h| std::fs::read_to_string(h.join(".claude.json")).ok()).unwrap_or_default();
            claude_code_entry(&text)
        }
        Some(Config::Json(path)) => json_entry(&std::fs::read_to_string(path).unwrap_or_default()),
        Some(Config::CodexToml(path)) => toml_entry(&std::fs::read_to_string(path).unwrap_or_default()),
    };
    entry.map_or(State::NotLinked, |command| State::Linked { command })
}

/// Adds Kickstand to `app`; returns what to tell the user.
pub fn link(app: App) -> Result<String> {
    let (command, args) = server_command()?;
    match app.config().with_context(|| format!("no {} config location on this system", app.name()))? {
        Config::ClaudeCli => {
            let claude = find_on_path("claude").context("Claude Code's `claude` command isn't installed")?;
            if let State::Linked { .. } = state(app) {
                // Replaced, so it runs this copy of Kickstand.
                run(&claude, &["mcp", "remove", "-s", "user", NAME]).ok();
            }
            let mut add = vec!["mcp", "add", "-s", "user", NAME, "--", &command];
            add.extend(args.iter().map(String::as_str));
            run(&claude, &add)?;
        }
        Config::Json(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            write_config(&path, &text, &with_json_entry(&text, &command, &args)?)?;
        }
        Config::CodexToml(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            write_config(&path, &text, &with_toml_entry(&text, &command, &args)?)?;
        }
    }
    Ok(format!("linked: {} to use Kickstand in {}", app.restart(), app.name()))
}

/// Takes Kickstand out of `app`'s config.
pub fn unlink(app: App) -> Result<String> {
    let removed = match app.config().with_context(|| format!("no {} config location on this system", app.name()))? {
        Config::ClaudeCli => {
            let claude = find_on_path("claude").context("Claude Code's `claude` command isn't installed")?;
            run(&claude, &["mcp", "remove", "-s", "user", NAME])?;
            true
        }
        Config::Json(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            without_json_entry(&text)?.map(|updated| write_config(&path, &text, &updated)).transpose()?.is_some()
        }
        Config::CodexToml(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            without_toml_entry(&text)?.map(|updated| write_config(&path, &text, &updated)).transpose()?.is_some()
        }
    };
    Ok(if removed { format!("unlinked: {} for {} to drop Kickstand", app.restart(), app.name()) } else { format!("{} didn't have Kickstand", app.name()) })
}

/// Kickstand's command in a `~/.claude.json`, user scope first, then any project's.
fn claude_code_entry(text: &str) -> Option<String> {
    let config: Value = serde_json::from_str(text).ok()?;
    let user = config.get("mcpServers").and_then(|s| s.get(NAME));
    let project = || config.get("projects")?.as_object()?.values().find_map(|p| p.get("mcpServers")?.get(NAME));
    user.or_else(project).map(describe)
}

fn json_entry(text: &str) -> Option<String> {
    let config: Value = serde_json::from_str(text).ok()?;
    config.get("mcpServers")?.get(NAME).map(describe)
}

/// `command args…` of a server entry.
fn describe(entry: &Value) -> String {
    let command = entry.get("command").and_then(Value::as_str).unwrap_or("?");
    let args = entry.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    std::iter::once(command).chain(args).collect::<Vec<_>>().join(" ")
}

/// `text` (a JSON config with `mcpServers`, possibly empty) with Kickstand's entry set.
fn with_json_entry(text: &str, command: &str, args: &[String]) -> Result<String> {
    let mut config = parse_object(text)?;
    let servers = config.entry("mcpServers").or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else { bail!("its mcpServers isn't an object; fix it by hand") };
    servers.insert(NAME.into(), json!({ "command": command, "args": args }));
    Ok(serde_json::to_string_pretty(&config)? + "\n")
}

/// `text` without Kickstand's entry, or `None` if it had none.
fn without_json_entry(text: &str) -> Result<Option<String>> {
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

/// Kickstand's command in a Codex `config.toml`.
fn toml_entry(text: &str) -> Option<String> {
    let doc: toml_edit::DocumentMut = text.parse().ok()?;
    let entry = doc.get("mcp_servers")?.get(NAME)?;
    let command = entry.get("command")?.as_str()?;
    let args = entry.get("args").and_then(|a| a.as_array()).into_iter().flatten().filter_map(|a| a.as_str());
    Some(std::iter::once(command).chain(args).collect::<Vec<_>>().join(" "))
}

/// `text` (a Codex `config.toml`, possibly empty) with `[mcp_servers.kickstand]` set.
fn with_toml_entry(text: &str, command: &str, args: &[String]) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("the config isn't valid TOML; fix it by hand")?;
    let servers = doc.entry("mcp_servers").or_insert_with(|| {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        toml_edit::Item::Table(table)
    });
    let Some(servers) = servers.as_table_mut() else { bail!("its mcp_servers isn't a table; fix it by hand") };
    let mut entry = toml_edit::Table::new();
    entry.insert("command", toml_edit::value(command));
    entry.insert("args", toml_edit::value(args.iter().map(String::as_str).collect::<toml_edit::Array>()));
    servers.insert(NAME, toml_edit::Item::Table(entry));
    Ok(doc.to_string())
}

/// `text` without `[mcp_servers.kickstand]`, or `None` if it had none.
fn without_toml_entry(text: &str) -> Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("the config isn't valid TOML; fix it by hand")?;
    let removed = doc.get_mut("mcp_servers").and_then(|s| s.as_table_like_mut()).and_then(|s| s.remove(NAME));
    Ok(removed.map(|_| doc.to_string()))
}

/// Writes through a temporary file, after saving the original once as `<name>.before-kickstand`.
fn write_config(path: &Path, old: &str, new: &str) -> Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let backup = path.with_file_name(format!("{name}.before-kickstand"));
    if !old.is_empty() && !backup.exists() {
        std::fs::write(&backup, old).with_context(|| format!("backing up {}", path.display()))?;
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let staged = path.with_file_name(format!("{name}.kickstand"));
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
        let linked = with_json_entry(config, "/opt/kickstand", &["serve".into()]).unwrap();
        let value: Value = serde_json::from_str(&linked).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(json_entry(&linked).as_deref(), Some("/opt/kickstand serve"));

        let unlinked = without_json_entry(&linked).unwrap().unwrap();
        assert_eq!(json_entry(&unlinked), None);
        assert_eq!(serde_json::from_str::<Value>(&unlinked).unwrap()["mcpServers"]["other"]["command"], "x");
        assert_eq!(without_json_entry(&unlinked).unwrap(), None);
    }

    #[test]
    fn desktop_config_from_nothing_and_bad_files() {
        let linked = with_json_entry("", "k", &["serve".into()]).unwrap();
        assert_eq!(json_entry(&linked).as_deref(), Some("k serve"));
        assert!(with_json_entry("{not json", "k", &[]).is_err());
        assert!(with_json_entry(r#"{"mcpServers":3}"#, "k", &[]).is_err());
    }

    #[test]
    fn codex_config_keeps_everything_else() {
        let config = "# mine\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let linked = with_toml_entry(config, "/opt/kickstand", &["serve".into()]).unwrap();
        assert!(linked.starts_with("# mine\nmodel = \"o3\""), "{linked}");
        assert!(linked.contains("[mcp_servers.kickstand]\ncommand = \"/opt/kickstand\"\nargs = [\"serve\"]"), "{linked}");
        assert_eq!(toml_entry(&linked).as_deref(), Some("/opt/kickstand serve"));
        assert_eq!(toml_entry(&linked.replace("kickstand]", "kickstand2]")), None);

        let unlinked = without_toml_entry(&linked).unwrap().unwrap();
        assert_eq!(toml_entry(&unlinked), None);
        assert!(unlinked.contains("[mcp_servers.other]"));
        assert_eq!(without_toml_entry(&unlinked).unwrap(), None);

        let fresh = with_toml_entry("", "k", &["serve".into()]).unwrap();
        assert_eq!(fresh, "[mcp_servers.kickstand]\ncommand = \"k\"\nargs = [\"serve\"]\n");
        assert!(with_toml_entry("mcp_servers = 3", "k", &[]).is_err());
    }

    #[test]
    fn apps_by_name() {
        assert_eq!(App::from_slug("Desktop"), Some(App::ClaudeDesktop));
        assert_eq!(App::from_slug("claude-code"), Some(App::ClaudeCode));
        assert_eq!(App::from_slug("lm-studio"), Some(App::LmStudio));
        assert_eq!(App::from_slug("gemini"), Some(App::GeminiCli));
        assert_eq!(App::from_slug("chatgpt"), None);
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
