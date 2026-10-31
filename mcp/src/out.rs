// SPDX-License-Identifier: GPL-3.0-only
//! The CLI's output: text for people or, with `--json`, one JSON object per command for scripts.
//!
//! Every object has `"ok": true` plus the command's fields; a failure prints `{"ok": false, "error": "…"}`
//! and exits with 1. Lines a command reports along the way go in `"notes"`. Commands that stream (`run`,
//! `tail -f`) print JSON Lines instead: one object per line as things happen. Title and program ids are
//! hex strings (`"0100000000001000"`), since JSON numbers lose precision above 2^53.

use std::cell::RefCell;
use std::io::Write;

use serde_json::{Map, Value, json};

pub struct Out {
    json: bool,
    notes: RefCell<Vec<String>>,
}

impl Out {
    pub fn new(json: bool) -> Self {
        Out { json, notes: RefCell::new(Vec::new()) }
    }

    pub fn json(&self) -> bool {
        self.json
    }

    /// A line along the way: printed now as text; with `--json`, kept for the result's `"notes"`.
    pub fn note(&self, line: impl Into<String>) {
        let line = line.into();
        if self.json { self.notes.borrow_mut().push(line) } else { println!("{line}") }
    }

    /// The command's result: `text()` for people, or `value` (an object) with `"ok": true` and any notes.
    pub fn done(&self, value: Value, text: impl FnOnce() -> String) {
        if self.json {
            let mut all = Map::new();
            all.insert("ok".into(), Value::Bool(true));
            if let Value::Object(fields) = value {
                all.extend(fields);
            }
            let notes = std::mem::take(&mut *self.notes.borrow_mut());
            if !notes.is_empty() {
                all.insert("notes".into(), json!(notes));
            }
            println!("{}", Value::Object(all));
        } else {
            let text = text();
            if text.ends_with('\n') || text.is_empty() { print!("{text}") } else { println!("{text}") }
        }
    }

    /// One event of a streaming command (JSON Lines), with the notes so far.
    pub fn event(&self, value: Value) {
        for note in std::mem::take(&mut *self.notes.borrow_mut()) {
            println!("{}", json!({ "note": note }));
        }
        println!("{value}");
        let _ = std::io::stdout().flush();
    }

    /// A failure, as `{"ok": false, "error": "…"}`.
    pub fn error(error: &anyhow::Error) {
        println!("{}", json!({ "ok": false, "error": format!("{error:#}") }));
    }
}

/// A title or program id as JSON: 16 hex digits.
pub fn id(id: u64) -> Value {
    Value::String(format!("{id:016x}"))
}

pub fn app(app: &proto::App) -> Value {
    json!({ "pid": app.pid, "program_id": id(app.program_id), "hbl": app.hbl })
}

/// The word a supervise or test report starts with, as an outcome: `"crashed"`, `"exited"`,
/// `"looks_hung"`, `"still_running"`, ... (`null` if it starts with something else).
pub fn outcome(text: &str) -> Value {
    let head: String = text.chars().take_while(|c| c.is_ascii_uppercase() || *c == ' ').collect();
    let head = head.trim();
    if head.len() < 4 { Value::Null } else { Value::String(head.to_lowercase().replace(' ', "_")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_and_ids() {
        assert_eq!(outcome("STILL RUNNING after 60 s: no crash"), json!("still_running"));
        assert_eq!(outcome("LOOKS HUNG after 20 s: ..."), json!("looks_hung"));
        assert_eq!(outcome("CRASHED after 3 s (/atmosphere/...)"), json!("crashed"));
        assert_eq!(outcome("A thing happened"), Value::Null);
        assert_eq!(id(0x0100000000001000), json!("0100000000001000"));
    }
}
