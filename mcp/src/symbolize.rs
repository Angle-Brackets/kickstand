// SPDX-License-Identifier: GPL-3.0-only
//! Turns `Module+0x1234` addresses (homebrew backtraces, Atmosphère crash reports) into functions and source
//! lines, from the module's ELF (its DWARF). An NRO's ELF is linked at 0, so the offset is the address.

use std::path::Path;

use anyhow::{Context, Result};

/// A symbolizer shared between tasks (the loader caches lazily, so it isn't `Sync`).
pub type Shared = std::sync::Arc<std::sync::Mutex<Symbolizer>>;

pub struct Symbolizer {
    loader: addr2line::Loader,
    /// Matched case-insensitively against the name before `+0x`.
    module: String,
}

impl Symbolizer {
    /// `module` defaults to the ELF's file name without its extension (`Paperboat.elf` → `Paperboat`).
    pub fn new(elf: &Path, module: Option<&str>) -> Result<Self> {
        let loader = addr2line::Loader::new(elf).map_err(|e| anyhow::anyhow!("{e}")).with_context(|| format!("loading {}", elf.display()))?;
        let module = match module {
            Some(m) => m.to_string(),
            None => elf.file_stem().context("ELF path has no file name")?.to_string_lossy().into_owned(),
        };
        Ok(Symbolizer { loader, module })
    }

    pub fn shared(self) -> Shared {
        std::sync::Arc::new(std::sync::Mutex::new(self))
    }

    /// `function at file:line`, innermost inlined frame first, joined with " / inlined into ".
    pub fn lookup(&self, offset: u64) -> String {
        let mut parts = Vec::new();
        if let Ok(mut frames) = self.loader.find_frames(offset) {
            while let Ok(Some(frame)) = frames.next() {
                let function = frame
                    .function
                    .as_ref()
                    .and_then(|f| f.demangle().ok().map(|n| n.into_owned()))
                    // Assembly (libnx's svc stubs) has line info but no DWARF function: use the symbol.
                    .or_else(|| self.loader.find_symbol(offset).map(|n| addr2line::demangle_auto(n.into(), None).into_owned()))
                    .unwrap_or_else(|| "??".to_string());
                let location = frame
                    .location
                    .map(|l| format!("{}:{}", l.file.unwrap_or("??"), l.line.unwrap_or(0)))
                    .unwrap_or_else(|| "??".to_string());
                parts.push(format!("{function} at {location}"));
            }
        }
        if parts.is_empty()
            && let Some(name) = self.loader.find_symbol(offset)
        {
            parts.push(addr2line::demangle_auto(name.into(), None).into_owned());
        }
        if parts.is_empty() { "??".to_string() } else { parts.join(" / inlined into ") }
    }

    /// Every `offset` in `line` written as `<module>+0x<hex>` or `<module> + 0x<hex>`.
    pub fn offsets_in(&self, line: &str) -> Vec<u64> {
        let lower = line.to_ascii_lowercase();
        let module = self.module.to_ascii_lowercase();
        let mut found = Vec::new();
        let mut from = 0;
        while let Some(at) = lower[from..].find(&module) {
            let start = from + at;
            from = start + module.len();
            // A whole word: not the tail of a longer name.
            if start > 0 && lower.as_bytes()[start - 1].is_ascii_alphanumeric() {
                continue;
            }
            let rest = lower[from..].trim_start();
            let Some(rest) = rest.strip_prefix('+') else { continue };
            let Some(hex) = rest.trim_start().strip_prefix("0x") else { continue };
            let end = hex.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(hex.len());
            if let Ok(offset) = u64::from_str_radix(&hex[..end], 16) {
                found.push(offset);
            }
        }
        found
    }

    /// `text` with a `    -> function at file:line` line after each line that has module offsets.
    pub fn annotate(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            out.push_str(line);
            let offsets = self.offsets_in(line);
            if !offsets.is_empty() && !line.ends_with('\n') {
                out.push('\n');
            }
            for offset in offsets {
                out.push_str(&format!("        -> {}\n", self.lookup(offset)));
            }
        }
        out
    }
}
