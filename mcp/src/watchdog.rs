// SPDX-License-Identifier: GPL-3.0-only
//! Watching running homebrew until something happens: it exits, crashes (a new crash report), takes the
//! console down (a fatal error), or hangs (no output and a frozen screen), with what's needed to see why:
//! the crash report symbolized, or every thread's backtrace and a screenshot.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::capture::{self, Until};
use crate::client::Client;
use crate::launch::OutputLog;
use crate::memory::Elf;

pub struct Settings {
    /// Give up watching (the homebrew is fine) after this long.
    pub max: Duration,
    /// No output for this long makes the screen get checked for a hang.
    pub quiet: Duration,
    /// On a fatal error, reboot into hekate entry 1 (back into the same system) straight away.
    pub reboot_on_fatal: bool,
}

/// How long the screen must stay unchanged, once the output has gone quiet, to call it a hang.
const FROZEN: Duration = Duration::from_secs(5);
/// How long creport gets to write a report once the process is gone.
const REPORT_WAIT: Duration = Duration::from_secs(10);
/// Output lines shown with a report.
const TAIL_LINES: usize = 15;

pub struct Report {
    pub text: String,
    pub screenshot: Option<Vec<u8>>,
}

/// Watches the homebrew running as the application (full memory) or, failing that, in the Album, with its
/// output in `output` if `run_nro` started it here.
pub async fn supervise(client: &Client, output: Option<&Arc<Mutex<OutputLog>>>, elf: &Elf, settings: &Settings) -> Result<Report> {
    let start = Instant::now();
    let status = client.status().await?;
    let app = match (status.application, status.applet) {
        (Some(app), _) if app.hbl == Some(true) => app,
        (_, Some(applet)) if applet.hbl == Some(true) => applet,
        _ => bail!("no homebrew is running (hbloader isn't the application or in the Album)"),
    };
    let known = crate::crash::report_paths(client, app.program_id).await?;
    let tail = || output.map(|o| o.lock().unwrap().tail(TAIL_LINES)).unwrap_or_default();
    let with_tail = |text: String| {
        let tail = tail();
        if tail.is_empty() { text } else { format!("{text}\n\n--- last output ---\n{tail}") }
    };
    // When it was last seen alive: output, or the screen changing.
    let mut alive = start;
    loop {
        if let Some(fatal) = client.fatal().await? {
            let mut text = fatal;
            if settings.reboot_on_fatal {
                client.reboot_to_hekate(proto::HekateTarget::Entry(1)).await?;
                text.push_str("\n\nRebooted into hekate entry 1; the daemon is back in about 3 minutes (check status).");
            }
            return Ok(Report { text: with_tail(text), screenshot: None });
        }

        let now = client.status().await?;
        let running = [now.application, now.applet].into_iter().flatten().any(|a| a.pid == app.pid);
        // creport writes the report a moment after the process is gone.
        let wait_for_report = if running { Duration::ZERO } else { REPORT_WAIT };
        let gone_at = Instant::now();
        loop {
            let new_crash = crate::crash::report_paths(client, app.program_id).await?.into_iter().find(|p| !known.contains(p));
            if let Some(path) = new_crash {
                let fetched = crate::crash::fetch(client, &path).await?;
                let symbols = elf.symbolizer()?;
                let text = format!("CRASHED after {:.0} s ({path}):\n\n{}", start.elapsed().as_secs_f64(), crate::crash::describe(&fetched, symbols.as_ref())?);
                return Ok(Report { text: with_tail(text), screenshot: fetched.jpg });
            }
            if gone_at.elapsed() >= wait_for_report {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !running {
            // An NRO that exits normally returns to hbmenu in the same process, so the process ending is a
            // crash the runtime handled itself (no report), or an exit to the home menu.
            let text = format!(
                "ENDED after {:.0} s: the process (pid {}) is gone and no crash report was written. An NRO that exits \
                 normally goes back to hbmenu in the same process, so this is most likely a crash its runtime handled \
                 (the screenshot shows any error dialog; A dismisses it)",
                start.elapsed().as_secs_f64(),
                app.pid
            );
            return Ok(Report { text: with_tail(text), screenshot: client.screenshot().await.ok() });
        }
        // Only a clean close is the NRO exiting; a dropped connection leaves it running, watched by its screen.
        if output.is_some_and(|o| o.lock().unwrap().end() == Some(&crate::launch::End::Closed)) {
            let text = format!("EXITED after {:.0} s: the NRO closed its stdout (back to hbmenu, or exiting)", start.elapsed().as_secs_f64());
            return Ok(Report { text: with_tail(text), screenshot: None });
        }

        if let Some(at) = output.and_then(|o| o.lock().unwrap().last_output()) {
            alive = alive.max(at);
        }
        if alive.elapsed() >= settings.quiet {
            // An application that never drew anything can't be captured (caps:sc 2206-0112): then the
            // output alone decides.
            let (changed, screenshot, screen) = match capture::wait(client, Until::Change, None, FROZEN).await {
                Ok(waited) => (waited.met, Some(waited.jpeg), "an unchanged screen"),
                Err(_) => (false, None, "a screen that can't be captured"),
            };
            if changed {
                alive = Instant::now();
            } else if output.is_some() || screenshot.is_some() {
                let quiet = alive.elapsed().as_secs_f64();
                let threads = crate::memory::threads(client, elf).await?;
                let dropped = output.and_then(|o| o.lock().unwrap().end().cloned()).is_some_and(|end| end != crate::launch::End::Closed);
                let what = match output {
                    Some(_) if dropped => format!("{screen} (its stdout connection dropped earlier, so output can't show it's alive)"),
                    Some(_) => format!("no output and {screen}"),
                    None => screen.to_string(),
                };
                let text = format!("LOOKS HUNG after {:.0} s: {what} for {quiet:.0} s. Every thread's backtrace:\n\n{threads}", start.elapsed().as_secs_f64());
                return Ok(Report { text: with_tail(text), screenshot });
            }
        }

        if start.elapsed() >= settings.max {
            let text = format!("STILL RUNNING after {:.0} s: no crash, fatal or hang (pid {})", start.elapsed().as_secs_f64(), app.pid);
            return Ok(Report { text: with_tail(text), screenshot: None });
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
