// SPDX-License-Identifier: GPL-3.0-only
//! Macros: controller input the user recorded once (from the desktop app, `kickstand macro record`, or an
//! agent's record_macro), saved on this PC and replayed by agents to get back to the same place in a game.
//! Each lives in `~/.config/kickstand/macros/<name>.json`, its input written in the `input` tool's step
//! syntax, so it can be read and edited by hand.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use proto::{Button, InputStep, MAX_PLAY_STEPS, RecordedInput, STICK_MAX};
use serde::{Deserialize, Serialize};

use crate::client::Client;

/// Most milliseconds of input in one [`proto::Request::Play`] (the daemon allows 20 s; this leaves room).
const CHUNK_MS: u32 = 15_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Macro {
    /// Seconds since 1970, when it was recorded.
    pub recorded: u64,
    /// Its length in milliseconds.
    pub ms: u64,
    /// What it does, if anyone said ("title screen to Rogueport, file 1").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The input, in the `input` tool's step syntax.
    pub steps: String,
}

/// A macro by name, for listings.
pub struct Listed {
    pub name: String,
    pub data: Macro,
}

fn dir() -> Result<PathBuf> {
    Ok(crate::config::config_dir()?.join("macros"))
}

/// Names are file names: letters, digits, spaces, `-` and `_`, up to 64.
pub fn check_name(name: &str) -> Result<&str> {
    let name = name.trim();
    let ok = !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'));
    if !ok {
        bail!("a macro's name is 1-64 letters, digits, spaces, - or _ (not {name:?})");
    }
    Ok(name)
}

fn path(name: &str) -> Result<PathBuf> {
    Ok(dir()?.join(format!("{}.json", check_name(name)?)))
}

pub fn load(name: &str) -> Result<Macro> {
    let path = path(name)?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("no macro {name:?} ({})", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

pub fn save(name: &str, data: &Macro) -> Result<PathBuf> {
    let path = path(name)?;
    std::fs::create_dir_all(dir()?)?;
    std::fs::write(&path, serde_json::to_string_pretty(data)? + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub fn delete(name: &str) -> Result<()> {
    let path = path(name)?;
    std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))
}

/// Every saved macro, by name.
pub fn list() -> Result<Vec<Listed>> {
    let Ok(entries) = std::fs::read_dir(dir()?) else { return Ok(Vec::new()) };
    let mut all: Vec<Listed> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.path().file_stem()?.to_string_lossy().into_owned();
            let data = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            e.path().extension().is_some_and(|x| x == "json").then_some(Listed { name, data })
        })
        .collect();
    all.sort_by_key(|m| m.name.to_lowercase());
    Ok(all)
}

/// A recording as a macro: idle time before the first input and after the last is left out.
pub fn from_recording(inputs: &[RecordedInput], ms: u32, note: String) -> Result<Macro> {
    let neutral = |i: &RecordedInput| i.buttons == 0 && i.left == (0, 0) && i.right == (0, 0);
    let Some(first) = inputs.iter().position(|i| !neutral(i)) else {
        bail!("nothing was pressed while recording (HOME and Capture aren't recorded)");
    };
    let mut steps: Vec<(RecordedInput, u32)> = Vec::new();
    for (i, input) in inputs.iter().enumerate().skip(first) {
        let end = inputs.get(i + 1).map_or(ms, |next| next.at_ms);
        let len = end.saturating_sub(input.at_ms);
        if len == 0 {
            continue;
        }
        match steps.last_mut() {
            Some((last, last_len)) if (last.buttons, last.left, last.right) == (input.buttons, input.left, input.right) => *last_len += len,
            _ => steps.push((*input, len)),
        }
    }
    // A last state of nothing held is the user putting the controller down.
    while steps.last().is_some_and(|(i, _)| neutral(i)) {
        steps.pop();
    }
    let total: u64 = steps.iter().map(|(_, len)| u64::from(*len)).sum();
    let text: Vec<String> = steps.iter().map(|(input, len)| step_text(input, *len)).collect();
    let recorded = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    Ok(Macro { recorded, ms: total, note, steps: text.join(" ") })
}

/// One step in the `input` syntax: "a+zl+l=0.50,-1:120", or "-:400" for nothing held.
fn step_text(input: &RecordedInput, ms: u32) -> String {
    let mut parts: Vec<String> = Button::ALL.iter().filter(|b| u64::from(input.buttons) & b.bit() != 0).map(|b| b.name().to_string()).collect();
    let axis = |v: i32| {
        let f = f64::from(v) / f64::from(STICK_MAX);
        let text = format!("{f:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    if input.left != (0, 0) {
        parts.push(format!("l={},{}", axis(input.left.0), axis(input.left.1)));
    }
    if input.right != (0, 0) {
        parts.push(format!("r={},{}", axis(input.right.0), axis(input.right.1)));
    }
    if parts.is_empty() {
        parts.push("-".into());
    }
    format!("{}:{ms}", parts.join("+"))
}

/// Steps split into [`proto::Request::Play`]s: at most [`MAX_PLAY_STEPS`] and [`CHUNK_MS`] each, a step
/// too long for one carried on in the next.
pub fn chunks(steps: Vec<InputStep>) -> Vec<Vec<InputStep>> {
    let mut chunks = Vec::new();
    let (mut chunk, mut chunk_ms) = (Vec::new(), 0u32);
    for mut step in steps {
        while step.ms > 0 {
            if chunk.len() == MAX_PLAY_STEPS || chunk_ms == CHUNK_MS {
                chunks.push(std::mem::take(&mut chunk));
                chunk_ms = 0;
            }
            let ms = step.ms.min(CHUNK_MS - chunk_ms);
            chunk.push(InputStep { ms, ..step.clone() });
            chunk_ms += ms;
            step.ms -= ms;
        }
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Plays macro `name`; returns how long it took.
pub async fn play(client: &Client, name: &str) -> Result<(Macro, Duration)> {
    let data = load(name)?;
    let steps = crate::client::parse_steps(&data.steps).with_context(|| format!("macro {name:?}'s steps"))?;
    let start = std::time::Instant::now();
    client.play(chunks(steps)).await?;
    Ok((data, start.elapsed()))
}

/// "1m 05s" or "12.3 s".
pub fn length(ms: u64) -> String {
    if ms >= 60_000 { format!("{}m {:02}s", ms / 60_000, ms % 60_000 / 1000) } else { format!("{:.1} s", ms as f64 / 1000.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(at_ms: u32, buttons: u32, left: (i32, i32)) -> RecordedInput {
        RecordedInput { at_ms, buttons, left, right: (0, 0) }
    }

    #[test]
    fn a_recording_becomes_steps_without_idle_ends() {
        let a = Button::A.bit() as u32;
        let inputs = [at(0, 0, (0, 0)), at(500, a, (0, 0)), at(600, a, (0, 0)), at(650, 0, (0, STICK_MAX)), at(2650, 0, (0, 0))];
        let m = from_recording(&inputs, 4000, String::new()).unwrap();
        assert_eq!(m.steps, "a:150 l=0,1:2000");
        assert_eq!(m.ms, 2150);
        let parsed = crate::client::parse_steps(&m.steps).unwrap();
        assert_eq!(parsed[1].left, Some((0, STICK_MAX)));
    }

    #[test]
    fn half_way_sticks_and_pauses_read_back() {
        let inputs = [at(0, 0, (STICK_MAX / 2, -STICK_MAX)), at(100, 0, (0, 0)), at(300, Button::B.bit() as u32, (0, 0))];
        let m = from_recording(&inputs, 400, String::new()).unwrap();
        assert_eq!(m.steps, "l=0.5,-1:100 -:200 b:100");
    }

    #[test]
    fn nothing_pressed_is_an_error() {
        assert!(from_recording(&[at(0, 0, (0, 0))], 1000, String::new()).is_err());
    }

    #[test]
    fn long_steps_split_across_chunks() {
        let step = |ms| InputStep { buttons: vec![Button::Right], ms, ..Default::default() };
        let chunks = chunks(vec![step(10_000), step(40_000)]);
        let lens: Vec<Vec<u32>> = chunks.iter().map(|c| c.iter().map(|s| s.ms).collect()).collect();
        assert_eq!(lens, [vec![10_000, 5_000], vec![15_000], vec![15_000], vec![5_000]]);
        let many: Vec<InputStep> = (0..600).map(|_| step(10)).collect();
        assert_eq!(super::chunks(many).iter().map(Vec::len).collect::<Vec<_>>(), [256, 256, 88]);
    }

    #[test]
    fn names_are_file_names() {
        assert!(check_name("rogueport from title").is_ok());
        assert!(check_name("../x").is_err());
        assert!(check_name("").is_err());
    }
}
