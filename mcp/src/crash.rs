// SPDX-License-Identifier: GPL-3.0-only
//! Reading what Atmosphère leaves after a crash, for the agent:
//! - `/atmosphere/crash_reports/<time>_<program id>.log` (+ `.jpg`): a process crashed. Text; summarized
//!   here to the crashed thread, its backtrace and the modules, with `Module + 0x...` addresses
//!   symbolized when an ELF is given.
//! - `/atmosphere/fatal_errors/report_<tick>.bin`: the whole console stopped on Atmosphère's fatal screen.
//!   A binary `ams::impl::FatalErrorContext` (layout from Atmosphère's `ams_fatal_error_context.hpp`).

use std::fmt::Write as _;

use anyhow::{Result, bail};

use crate::client::Client;
use crate::symbolize::Symbolizer;

pub const CRASH_DIR: &str = "/atmosphere/crash_reports";
pub const FATAL_DIR: &str = "/atmosphere/fatal_errors";

/// The crash report's summary: everything but the register, TLS and stack dumps and the other threads.
pub fn summarize_crash(report: &str, symbols: Option<&Symbolizer>) -> String {
    let mut out = String::new();
    let mut section = "";
    let mut crashed_id = None;
    let mut in_crashed_thread = false;
    for line in report.lines() {
        let trimmed = line.trim();
        let field = |name: &str| trimmed.strip_prefix(name).map(|v| v.trim().to_string());
        if !line.starts_with(' ') && trimmed.ends_with(':') {
            section = trimmed;
        }
        let keep = match section {
            "Process Info:" | "Exception Info:" | "Module Info:" => !trimmed.is_empty(),
            "Crashed Thread Info:" => {
                if let Some(id) = field("Thread ID:") {
                    crashed_id = Some(id);
                }
                is_summary_line(trimmed)
            }
            "Thread Report:" => {
                // The crashed thread again, now with its backtrace (the "Crashed Thread Info" section has none).
                if let Some(id) = field("Thread ID:") {
                    in_crashed_thread = crashed_id.as_deref() == Some(id.as_str());
                }
                in_crashed_thread && trimmed.starts_with("ReturnAddress")
            }
            _ => trimmed.starts_with("Atmosphère Crash Report") || trimmed.starts_with("Result:"),
        };
        if keep && !trimmed.ends_with("0000000000000000") {
            if trimmed.starts_with("ReturnAddress") && !out.contains("    Stack Trace:") {
                out.push_str("    Stack Trace:\n");
            }
            out.push_str(line);
            out.push('\n');
            if let Some(symbols) = symbols {
                for offset in symbols.offsets_in(line) {
                    let _ = writeln!(out, "        -> {}", symbols.lookup(offset));
                }
            }
        }
    }
    out
}

fn is_summary_line(trimmed: &str) -> bool {
    ["Crashed Thread Info:", "Thread ID:", "PC:", "LR:", "SP:", "FP:"].iter().any(|p| trimmed.starts_with(p))
}

/// `report_<tick>.bin` from exosphère: which program brought the console down, and how.
pub fn describe_fatal(bin: &[u8]) -> Result<String> {
    if bin.len() < 0x450 || &bin[..4] != b"AFE2" {
        bail!("not an Atmosphère fatal error context (AFE2, 0x450 bytes)");
    }
    let u32_at = |o: usize| u32::from_le_bytes(bin[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(bin[o..o + 8].try_into().unwrap());
    let desc = u32_at(4);
    let mut out = String::new();
    let _ = writeln!(out, "Fatal error: {} ({desc:#x})", fatal_desc(desc));
    let _ = writeln!(out, "Program: {:016x}{}", u64_at(8), program_name(u64_at(8)).map(|n| format!(" ({n})")).unwrap_or_default());
    let pc = u64_at(0x110);
    let base = u64_at(0x118);
    let _ = writeln!(out, "PC: {pc:016x} (module base {base:016x}, + {:#x})", pc.wrapping_sub(base));
    let _ = writeln!(out, "LR: {:016x}  SP: {:016x}  FP: {:016x}", u64_at(0x100), u64_at(0x108), u64_at(0xF8));
    let _ = writeln!(out, "ESR: {:08x}  FAR: {:016x}", u32_at(0x12C), u64_at(0x130));
    let x0 = u64_at(0x10);
    if x0 != 0 && x0 < 1 << 32 {
        let rc = x0 as u32;
        let _ = writeln!(out, "X0: {x0:#x} (as a result: {})", describe_result(rc));
    }
    let _ = writeln!(out, "Uptime at report: ~{} s", u64_at(0x138) / 19_200_000);
    let frames = (u64_at(0x140) as usize).min(0x20);
    if frames > 0 {
        out.push_str("Stack trace:\n");
        for i in 0..frames {
            let addr = u64_at(0x150 + i * 8);
            let _ = writeln!(out, "  {i:02}: {addr:016x} (+ {:#x})", addr.wrapping_sub(base));
        }
    }
    Ok(out)
}

/// The names Atmosphère's fatal screen gives these error descriptions.
fn fatal_desc(desc: u32) -> &'static str {
    match desc {
        0x100 => "instruction abort",
        0x101 => "data abort",
        0x102 => "PC misalignment",
        0x103 => "SP misalignment",
        0x104 => "trap",
        0x106 => "SError",
        0x301 => "bad SVC",
        0xF00 => "kernel panic",
        0xFFD => "stack overflow",
        0xFFE => "std::abort() called",
        _ => "unknown",
    }
}

/// System programs that show up in fatals.
fn program_name(id: u64) -> Option<&'static str> {
    Some(match id {
        0x0100000000000000 => "fs",
        0x0100000000000001 => "ldr",
        0x0100000000000002 => "ncm",
        0x0100000000000003 => "pm",
        0x0100000000000004 => "sm",
        0x0100000000000006 => "usb",
        0x0100000000000009 => "settings",
        0x010000000000000F => "wlan",
        0x0100000000000010 => "ldn",
        0x0100000000000012 => "bsdsockets",
        0x0100000000000013 => "hid",
        0x0100000000000016 => "nifm",
        0x0100000000000018 => "nvservices",
        0x0100000000000019 => "nim",
        0x0100000000000021 => "ns",
        0x0100000000000023 => "am",
        0x0100000000000024 => "ssl",
        0x0100000000000025 => "nim",
        0x010000000000002D => "vi",
        0x0100000000000034 => "fatal",
        0x010000000000003C => "jit",
        0x0100000000001000 => "qlaunch (home menu)",
        0x0100000000001023 => "error applet",
        0x010000000000000D => "dmnt",
        0x42000000004D4350 => "kickstand sysmodule",
        _ => return None,
    })
}

/// `2001-0132 (kernel: limit reached ...)`.
pub fn describe_result(rc: u32) -> String {
    let module = rc & 0x1FF;
    let desc = (rc >> 9) & 0x1FFF;
    let name = match (module, desc) {
        (1, 7) => Some("kernel: out of sessions"),
        (1, 101) => Some("kernel: invalid size"),
        (1, 102) => Some("kernel: invalid address"),
        (1, 103) => Some("kernel: out of resource"),
        (1, 104) => Some("kernel: out of memory"),
        (1, 105) => Some("kernel: out of handles"),
        (1, 114) => Some("kernel: invalid handle"),
        (1, 117) => Some("kernel: timed out"),
        (1, 123) => Some("kernel: session closed"),
        (1, 125) => Some("kernel: invalid state"),
        (1, 132) => Some("kernel: limit reached (the system ran out of threads, sessions, memory...)"),
        _ => None,
    };
    let code = format!("{:04}-{:04}", 2000 + module, desc);
    match name {
        Some(name) => format!("{code} ({name})"),
        None => code,
    }
}

/// A crash report or fatal record on the SD card.
pub struct Entry {
    pub path: String,
    /// One line: when, what and how.
    pub headline: String,
}

/// The newest `limit` crash reports, newest first, then up to `limit` fatal records (named by uptime
/// ticks, so they can't be placed in time; there are rarely more than a few).
pub async fn list(client: &Client, limit: usize) -> Result<Vec<Entry>> {
    let mut reports: Vec<(u64, String)> = Vec::new();
    let mut fatals: Vec<(u64, String)> = Vec::new();
    if let Ok((entries, _)) = client.list_dir(CRASH_DIR).await {
        for e in entries {
            if let Some(time) = e.name.strip_suffix(".log").and_then(|n| n.split('_').next()) {
                reports.push((time.parse().unwrap_or(0), format!("{CRASH_DIR}/{}", e.name)));
            }
        }
    }
    if let Ok((entries, _)) = client.list_dir(FATAL_DIR).await {
        for e in entries.into_iter().filter(|e| e.name.starts_with("report_") && e.name.ends_with(".bin")) {
            fatals.push((0, format!("{FATAL_DIR}/{}", e.name)));
        }
    }
    reports.sort_by(|a, b| b.cmp(a));
    reports.truncate(limit);
    fatals.truncate(limit);
    let mut out = Vec::new();
    for (time, path) in reports.into_iter().chain(fatals) {
        let headline = if path.ends_with(".bin") {
            let (bin, _) = client.read(&path, 0, 0x450).await?;
            describe_fatal(&bin).map_or_else(|e| format!("unreadable: {e}"), |d| {
                format!("console fatal: {}", d.lines().take(2).collect::<Vec<_>>().join(", "))
            })
        } else {
            let (head, _) = client.read(&path, 0, 2048).await?;
            let head = String::from_utf8_lossy(&head);
            let get = |name: &str| head.lines().find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string())).unwrap_or_default();
            format!(
                "{} UTC: {} ({}) crashed: {}, result {}",
                posix_time(time),
                get("Process Name:"),
                get("Program ID:"),
                get("Type:"),
                get("Result:"),
            )
        };
        out.push(Entry { path, headline });
    }
    Ok(out)
}

/// A crash report or fatal record as read from the SD card.
pub struct Fetched {
    path: String,
    data: Vec<u8>,
    /// The screenshot Atmosphère took with a crash report, if any.
    pub jpg: Option<Vec<u8>>,
}

/// Reads a crash report (by path or file name) and its screenshot, or a fatal record.
pub async fn fetch(client: &Client, path: &str) -> Result<Fetched> {
    let path = if path.starts_with('/') {
        path.to_string()
    } else if path.ends_with(".bin") {
        format!("{FATAL_DIR}/{path}")
    } else {
        format!("{CRASH_DIR}/{path}")
    };
    let (data, _) = client.read(&path, 0, 4 << 20).await?;
    let jpg = match path.strip_suffix(".log") {
        Some(stem) => client.read(&format!("{stem}.jpg"), 0, 1 << 20).await.ok().map(|(j, _)| j),
        None => None,
    };
    Ok(Fetched { path, data, jpg })
}

/// The crash report summarized, or the fatal record decoded.
pub fn describe(fetched: &Fetched, symbols: Option<&Symbolizer>) -> Result<String> {
    if fetched.path.ends_with(".bin") {
        return describe_fatal(&fetched.data);
    }
    Ok(summarize_crash(&String::from_utf8_lossy(&fetched.data), symbols))
}

/// `2053-08-01 08:52:38` (the console's clock, which is whatever the user set).
fn posix_time(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}
