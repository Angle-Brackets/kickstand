// SPDX-License-Identifier: GPL-3.0-only
//! Suggestions Kickstand puts to the user as a question (through the AI app, like the SD card access
//! questions): for now, linking libkickstand into a homebrew that reloads without it. Asked once per app per
//! Kickstand run; "Don't ask again" is kept in `~/.config/kickstand/hints.json`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// What a homebrew is written with, from its NRO, for which libkickstand fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// libnx (C or C++): the static library.
    C,
    /// The nx crate: the `kickstand-nx` crate.
    Rust,
    Unknown,
}

pub fn lang(nro: &[u8]) -> Lang {
    let has = |s: &[u8]| memchr::memmem::find(nro, s).is_some();
    if has(b"aarch64-switch-rs") {
        Lang::Rust
    } else if has(b"libnx") {
        Lang::C
    } else {
        Lang::Unknown
    }
}

/// An app's name for the questions and "don't ask again": its NRO's file name without `.nro` (a reload's
/// `.reload.nro` twin included), lowercase.
pub fn app_name(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let stem = file.strip_suffix(".nro").unwrap_or(&file);
    stem.strip_suffix(".reload").unwrap_or(stem).to_string()
}

/// The last build started in place of another in the same process, without libkickstand: if that process
/// crashes soon after, leftovers from the build before are the likely cause.
#[derive(Clone, Debug)]
pub struct InProcess {
    pub pid: u64,
    pub at: Instant,
    pub app: String,
    pub lang: Lang,
}

static LAST_IN_PROCESS: Mutex<Option<InProcess>> = Mutex::new(None);
/// How long after such a reload a crash points at it.
const CRASH_WINDOW: Duration = Duration::from_secs(120);

/// Notes a build that loaded into the same process as the one before it; `with_lib`: it links libkickstand
/// (then a crash isn't blamed on leftovers).
pub fn reloaded_in_process(pid: u64, app: &str, lang: Lang, with_lib: bool) {
    *LAST_IN_PROCESS.lock().unwrap() = (!with_lib).then(|| InProcess { pid, at: Instant::now(), app: app.into(), lang });
}

/// The reload a crash of `pid` may come from.
pub fn crash_after_reload(pid: u64) -> Option<InProcess> {
    LAST_IN_PROCESS.lock().unwrap().clone().filter(|r| r.pid == pid && r.at.elapsed() < CRASH_WINDOW)
}

/// Apps already asked about in this Kickstand run.
static ASKED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Whether to ask about `app` now: not yet in this run, and the user didn't say never.
pub fn should_ask(app: &str) -> bool {
    !ASKED.lock().unwrap().contains(app) && !saved().never.contains(app)
}

pub fn asked(app: &str) {
    ASKED.lock().unwrap().insert(app.to_string());
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    /// Apps the user doesn't want libkickstand suggested for.
    #[serde(default)]
    never: BTreeSet<String>,
    /// The user doesn't want to be asked to set up USB.
    #[serde(default)]
    never_usb_setup: bool,
}

fn file() -> Result<PathBuf> {
    Ok(crate::config::config_dir()?.join("hints.json"))
}

fn saved() -> Saved {
    file().ok().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Never suggests libkickstand for `app` again (the user can delete `hints.json` to undo it).
pub fn never(app: &str) -> Result<()> {
    asked(app);
    let mut saved = saved();
    saved.never.insert(app.to_string());
    save(&saved)
}

fn save(saved: &Saved) -> Result<()> {
    let path = file()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(saved)?)?;
    Ok(())
}

/// The question, for the user.
pub fn question(app: &str, lang: Lang, crashed: bool) -> String {
    let what = match lang {
        Lang::C => "a small static library plus a few linker flags in its Makefile or CMakeLists",
        Lang::Rust => "the kickstand-nx crate and three calls (quit check, thread spawn, finish)",
        Lang::Unknown => "a small library (C/C++ with libnx) or crate (Rust with nx)",
    };
    let lead = if crashed {
        format!(
            "Kickstand: {app} crashed soon after a reload into the same process. Leftovers from the build before \
             (threads still running, unclosed thread stacks, JIT memory) are the likely cause."
        )
    } else {
        format!("Kickstand: {app} reloads without libkickstand.")
    };
    format!(
        "{lead} Linking libkickstand into its build ({what}) makes reloads quit without a button press and \
         clears out what the old build leaves behind, which can crash the next one. Should the agent add it to \
         {app}'s build?"
    )
}

/// What the agent does once the user says yes.
pub fn how_to(app: &str, lang: Lang) -> String {
    let how = match lang {
        Lang::C => {
            "C/C++ (libnx): build libkickstand with `make -C <kickstand>/libkickstand`, then in the app's Makefile \
             `include <kickstand>/libkickstand/kickstand.mk`, `LDFLAGS += $(KICKSTAND_LDFLAGS)` and \
             `LIBS := -L<kickstand>/libkickstand -lkickstand $(LIBS)` (CMake: the same flags with \
             target_link_options and target_link_libraries; TOOLS.md#libkickstand has both). Nothing to call"
        }
        Lang::Rust => {
            "Rust (nx): add `kickstand-nx` (Kickstand's libkickstand/rust, by path or git) with its nx pointed at the \
             app's, call `kickstand::quit_requested()` in the main loop and quit when true, start threads with \
             `kickstand::spawn` instead of `nx::thread::spawn`, and call `kickstand::finish()` just before main \
             returns (TOOLS.md#libkickstand)"
        }
        Lang::Unknown => "See TOOLS.md#libkickstand for C/C++ (libnx) and Rust (nx)",
    };
    format!(
        "The user wants libkickstand linked into {app}'s build. {how}. Show the user the change, rebuild, and \
         reload_nro: the build running now doesn't have it yet, so that reload still exits it the old way; the \
         ones after are asked."
    )
}

/// For AI apps that can't show Kickstand's question: what the agent asks in the chat.
pub fn ask_in_chat(app: &str, lang: Lang, crashed: bool) -> String {
    format!(
        "Kickstand suggests linking libkickstand into {app}. Ask the user as a question with options (use your \
         app's question tool if it has one), don't decide for them: \"{}\" Options: \"Yes, add it to the build\", \
         \"Not now\", \"Don't ask again for {app}\". Then call answer_hint with app {app:?} and their answer (add, \
         not_now or never).",
        question(app, lang, crashed)
    )
}

// ---------------------------------------------------------------------------------------------------------
// The one-time USB setup (a udev rule on Linux, the WinUSB driver on Windows), asked about when an upload
// couldn't use USB for want of it.

static USB_ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether to ask about the USB setup now: once per Kickstand run, and not after "Don't ask again".
pub fn should_ask_usb() -> bool {
    !USB_ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) && !saved().never_usb_setup
}

pub fn never_usb() -> Result<()> {
    let mut saved = saved();
    saved.never_usb_setup = true;
    save(&saved)
}

/// The question: `change` says what the setup changes on this PC ([`crate::usb_setup::needed`]).
pub fn usb_question(change: &str) -> String {
    format!(
        "Kickstand: this PC isn't set up to send files to the Switch over USB yet, so they went over Wi-Fi \
         (about 7 MB/s; USB about 19). {change} Set it up now? Your system's own prompt asks you to approve it."
    )
}

/// For AI apps that can't show Kickstand's question: what the agent asks in the chat. It doesn't start the
/// setup itself: the user does, so the yes is theirs.
pub fn usb_ask_in_chat(change: &str) -> String {
    format!(
        "Kickstand suggests the one-time USB setup. Ask the user as a question with options (use your app's \
         question tool if it has one), don't decide for them: \"{}\" Options: \"I'll set it up\" (they run \
         `kickstand usb-setup` in their own terminal, or click Set up USB in Kickstand's desktop app; don't run it \
         for them), \"Use Wi-Fi for now\", \"Don't ask again\" (then call answer_hint with hint \"usb_setup\" \
         and answer \"never\").",
        usb_question(change)
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UsbSetupAnswer {
    /// Set up USB on this PC?
    pub choice: UsbSetupChoice,
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[schemars(inline)]
pub enum UsbSetupChoice {
    #[serde(rename = "Set it up now")]
    SetUp,
    #[serde(rename = "Use Wi-Fi for now")]
    Wifi,
    #[serde(rename = "Don't ask again")]
    Never,
}

rmcp::elicit_safe!(UsbSetupAnswer);

/// The user's answer to [`question`], shown as options by the AI app.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct LinkAnswer {
    /// Add libkickstand?
    pub choice: LinkChoice,
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[schemars(inline)]
pub enum LinkChoice {
    #[serde(rename = "Yes, add it to the build")]
    Add,
    #[serde(rename = "Not now")]
    NotNow,
    #[serde(rename = "Don't ask again for this app")]
    Never,
}

rmcp::elicit_safe!(LinkAnswer);

/// What the agent is told after the user answered (in the AI app's question or the chat); records a "never".
pub fn answered(app: &str, lang: Lang, choice: LinkChoice) -> Result<String> {
    asked(app);
    Ok(match choice {
        LinkChoice::Add => how_to(app, lang),
        LinkChoice::NotNow => format!("The user doesn't want libkickstand in {app} for now; Kickstand won't ask again until it restarts."),
        LinkChoice::Never => {
            never(app)?;
            format!("The user doesn't want libkickstand suggested for {app}; Kickstand won't ask again (~/.config/kickstand/hints.json).")
        }
    })
}

/// The language of the last build asked about per app, for [`answered`] from the chat.
static LANGS: Mutex<Vec<(String, Lang)>> = Mutex::new(Vec::new());

pub fn remember_lang(app: &str, lang: Lang) {
    let mut langs = LANGS.lock().unwrap();
    langs.retain(|(a, _)| a != app);
    langs.push((app.to_string(), lang));
}

pub fn lang_of(app: &str) -> Lang {
    LANGS.lock().unwrap().iter().find(|(a, _)| a == app).map_or(Lang::Unknown, |(_, l)| *l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_libnx_from_nx_and_names_apps() {
        assert_eq!(lang(b"..libnx.."), Lang::C);
        assert_eq!(lang(b"..aarch64-switch-rs (unknown module).."), Lang::Rust);
        assert_eq!(lang(b"NRO0"), Lang::Unknown);
        assert_eq!(app_name("/switch/paperboat/Paperboat.reload.nro"), "paperboat");
        assert_eq!(app_name("/switch/hello.nro"), "hello");
    }

    #[test]
    fn a_crash_points_at_the_last_reload_without_the_library_only() {
        reloaded_in_process(7, "game", Lang::C, false);
        assert!(crash_after_reload(7).is_some_and(|r| r.app == "game"));
        assert!(crash_after_reload(8).is_none());
        reloaded_in_process(7, "game", Lang::C, true);
        assert!(crash_after_reload(7).is_none());
    }
}
