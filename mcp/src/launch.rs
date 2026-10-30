// SPDX-License-Identifier: GPL-3.0-only
//! Opening hbmenu with nobody at the console: launch a title from the home menu while holding R, which
//! Atmosphère turns into hbloader with the title's full memory. Not every title works (ones that check
//! online rights, game cards that aren't inserted), and only the home menu's row can do it, not "All
//! Software". So the first time, try each tile of the row in order; remember the one that worked (the
//! host title, in `~/.config/kickstand/host-title`) and go straight to it afterwards.

use std::time::Duration;

use anyhow::{Result, bail};
use proto::{App, AppEntry, Button, HOME_MENU_TILES, InputStep};

use crate::client::Client;
use crate::config;

/// Shortest wait for the home menu to come back after an application closes.
const AFTER_CLOSE: Duration = Duration::from_millis(1500);
/// How long to look for a new application process after the last input.
const PROCESS_WAIT: Duration = Duration::from_secs(5);

pub struct Opened {
    pub host: AppEntry,
    /// What was tried on the way, for the agent to relay.
    pub notes: Vec<String>,
}

fn step(buttons: &[Button], ms: u32) -> InputStep {
    InputStep { buttons: buttons.to_vec(), ms }
}

/// Opens hbmenu, from whatever is running. Fails with what it tried if no title on the row works.
pub async fn open_hbmenu(client: &Client) -> Result<Opened> {
    let mut notes = Vec::new();
    if let Some(app) = client.app_status().await? {
        if app.hbl == Some(true) {
            let host = AppEntry { id: app.program_id, name: String::new() };
            notes.push("hbloader was already running (hbmenu or an NRO)".into());
            return Ok(Opened { host, notes });
        }
        close(client, &app).await?;
        notes.push(format!("closed the running title {:016x}", app.program_id));
    }

    let apps = client.list_apps(HOME_MENU_TILES as u16).await?;
    if apps.is_empty() {
        bail!("no titles are installed; one is needed to open hbmenu with full memory");
    }
    // The saved host title first, then the rest of the row in order.
    let saved = config::host_title();
    let mut order: Vec<usize> = (0..apps.len()).collect();
    if let Some(i) = saved.and_then(|id| apps.iter().position(|a| a.id == id)) {
        order.remove(i);
        order.insert(0, i);
    } else if let Some(id) = saved {
        notes.push(format!("the saved host title {id:016x} isn't on the home menu's row any more"));
    }

    for tile in order {
        let expected = &apps[tile];
        let label = if expected.name.is_empty() { format!("{:016x}", expected.id) } else { expected.name.clone() };
        match try_tile(client, tile).await? {
            Some(app) if app.hbl == Some(true) => {
                let host = apps.iter().find(|a| a.id == app.program_id).cloned().unwrap_or(AppEntry { id: app.program_id, name: String::new() });
                config::save_host_title(host.id, &host.name)?;
                notes.push(format!("hbmenu opened through {label} (tile {tile})"));
                return Ok(Opened { host, notes });
            }
            Some(app) => {
                let how = if app.hbl.is_none() { "Atmosphère didn't say whether it was hbloader" } else { "it started as the game" };
                notes.push(format!("{label} (tile {tile}): {how}"));
                close(client, &app).await?;
            }
            None => {
                notes.push(format!("{label} (tile {tile}): nothing started (it needs a card, rights or an online check)"));
                dismiss_dialogs(client).await?;
            }
        }
    }
    bail!("no title on the home menu's row opened hbmenu:\n- {}", notes.join("\n- "))
}

/// Starts tile `tile` of the home menu's row with R held; returns the application that started, if any.
async fn try_tile(client: &Client, tile: usize) -> Result<Option<App>> {
    // HOME on the home menu selects the first tile.
    let mut steps = vec![step(&[Button::Home], 100), step(&[], 1200)];
    for _ in 0..tile {
        steps.extend([step(&[Button::Right], 80), step(&[], 150)]);
    }
    client.input(steps).await?;
    // Start it, holding R long enough to cover the process starting. A title that asks who is playing
    // only starts once that's answered, so nothing is running yet: confirm the first user, still on R.
    let launch = vec![step(&[Button::R], 300), step(&[Button::R, Button::A], 150), step(&[Button::R], 6000)];
    client.input(launch.clone()).await?;
    if let Some(app) = client.app_status().await? {
        return Ok(Some(app));
    }
    client.input(launch).await?;
    wait_for_app(client).await
}

/// Backs out of what a title that didn't start leaves behind. Nintendo Switch Online, for one, chains
/// "Sign in to your Nintendo Account again", an error (2155-8007) a few seconds later, then the user
/// picker. B closes each, and does nothing once the home menu is back.
async fn dismiss_dialogs(client: &Client) -> Result<()> {
    let steps = (0..4).flat_map(|_| [step(&[Button::B], 100), step(&[], 3500)]).collect();
    client.input(steps).await
}

async fn wait_for_app(client: &Client) -> Result<Option<App>> {
    let start = tokio::time::Instant::now();
    loop {
        if let Some(app) = client.app_status().await? {
            return Ok(Some(app));
        }
        if start.elapsed() > PROCESS_WAIT {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Terminates `app` and waits for the home menu to take over again.
async fn close(client: &Client, app: &App) -> Result<()> {
    client.close_app().await?;
    let start = tokio::time::Instant::now();
    while client.app_status().await?.is_some_and(|now| now.pid == app.pid) {
        if start.elapsed() > PROCESS_WAIT {
            bail!("application {:016x} didn't close", app.program_id);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tokio::time::sleep(AFTER_CLOSE).await;
    Ok(())
}
