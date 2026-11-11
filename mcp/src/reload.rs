// SPDX-License-Identifier: GPL-3.0-only
//! Fast in-process NRO hot-reloading without a Home Menu round-trip.
//!
//! When an NRO running under `nx-hbloader` exits normally (for example when `+` is pressed in standard
//! `libnx` / `nx` homebrew), `hbloader` does **not** terminate the application process: it stays inside the
//! same full-memory process and loads the next NRO (`sdmc:/hbmenu.nro` by default).
//!
//! [`hot_reload`] exploits this to cut the 10–15 s Home Menu round-trip (`HOME -> X -> A -> re-arm hbl
//! override -> launch host title -> wait for hbloader -> NetLoader`) down to ~1–2 s:
//! 1. **Upload while running**: stream the new `.nro` directly to its destination on the SD card (over USB via
//!    [`crate::usb::upload`] when requested and available, or over Wi-Fi via [`Client::upload`]) while the old
//!    NRO is still running.
//! 2. **In-process exit to hbmenu**: press `+` (or caller-supplied `exit_buttons`) so the running NRO exits
//!    cleanly back to `hbmenu` inside the same `hbloader` PID.
//! 3. **NetLoader chainload**: probe [`launch::start_netloader`] (`Y` -> port 28280) and send the tiny
//!    `kickstand.nro` launcher with `--open <dest> [args...]` so `hbloader` immediately chainloads the new NRO
//!    from the SD card via `nx::hbl::set_next_load_entry`.
//! 4. **Seamless fallback**: if `hbloader` isn't running or the NRO doesn't exit to `hbmenu` (e.g. hung), fall
//!    back to [`launch::run_nro`] so the reload never fails.
//! 5. **Optional macro replay**: once the NRO starts, optionally replay a saved macro ([`crate::macros::play`])
//!    to return straight to the gameplay state under test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use proto::Button;

use crate::client::{Client, check_path};
use crate::launch::{self, Mode, Program, Started};
use crate::nxlink;

/// How long to let the running NRO tear down and hand back to `hbmenu` after pressing the exit button(s).
const EXIT_SETTLE: Duration = if cfg!(test) { Duration::from_millis(30) } else { Duration::from_millis(350) };

/// How long to wait for `hbmenu`'s NetLoader in the fast in-process path before falling back to a full relaunch.
const HOT_NETLOADER_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_millis(2000) };

/// How long to wait for the chainloaded NRO to connect its `nxlink` stdout in the hot path.
const HOT_OUTPUT_WAIT: Duration = if cfg!(test) { Duration::from_millis(200) } else { launch::OUTPUT_WAIT };

#[derive(Debug)]
pub struct ReloadOptions {
    pub program: launch::Program,
    pub args: Vec<String>,
    pub mode: launch::Mode,
    pub use_usb: bool,
    pub exit_buttons: Option<Vec<proto::Button>>,
    pub replay_macro: Option<String>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Reloaded {
    pub started: launch::Started,
    pub hot: bool,
    pub elapsed: Duration,
    pub transfer_note: String,
}

/// Builds the `--open <dest> [args...]` argument list for the Kickstand NRO launcher.
pub fn chainload_args(sd_dest: &str, args: &[String]) -> Vec<String> {
    ["--open".to_string(), sd_dest.to_string()].into_iter().chain(args.iter().cloned()).collect()
}

/// Resolves the buttons used to ask a running NRO to exit back to `hbmenu` (`Plus` by default).
pub fn default_exit_buttons(custom: Option<&[Button]>) -> Vec<Button> {
    match custom {
        Some(buttons) => buttons.to_vec(),
        None => vec![Button::Plus],
    }
}

/// Hot-reloads an NRO in-process when `hbloader` is running, falling back to [`launch::run_nro`] when needed.
pub async fn hot_reload(client: &Client, opts: ReloadOptions) -> Result<Reloaded> {
    let sd_dest = validate_destination(client, &opts.program, opts.mode).await?;
    if let Some(macro_name) = &opts.replay_macro {
        // Validate the macro up front before spending time transferring or reloading the NRO.
        crate::macros::load(macro_name)?;
    }

    let start = Instant::now();
    let transfer_note = stage_nro_on_sd(client, &opts.program, &sd_dest, opts.use_usb).await?;

    let (mut started, hot) = match try_in_process_reload(
        client,
        &sd_dest,
        &opts.args,
        opts.mode,
        opts.exit_buttons.as_deref(),
    )
    .await?
    {
        HotOutcome::Hot(mut started) => {
            started.notes.insert(0, transfer_note.clone());
            (started, true)
        }
        HotOutcome::Cold { reason } => {
            // If the Kickstand launcher NRO is on the SD card, use `Program::Sd` so `run_nro` doesn't
            // re-upload the NRO binary a second time over slow NetLoader TCP.
            let fallback_program = if client.stat(launch::LAUNCHER).await.is_ok() {
                Program::Sd(sd_dest.clone())
            } else {
                opts.program
            };
            let mut started = launch::run_nro(client, fallback_program, opts.args, opts.mode).await?;
            started.notes.insert(0, format!("in-process reload unavailable ({reason}); fell back to full relaunch"));
            started.notes.insert(0, transfer_note.clone());
            (started, false)
        }
    };

    if let Some(macro_name) = &opts.replay_macro {
        let replayed = crate::macros::play(client, macro_name).await?;
        started.notes.push(format!("replayed macro: {}", replayed.summary()));
    }

    Ok(Reloaded {
        started,
        hot,
        elapsed: start.elapsed(),
        transfer_note,
    })
}

/// Checks path rules, `.nro` extension, `/switch/` placement for local uploads, and the applet-mode game rule.
async fn validate_destination(client: &Client, program: &Program, mode: Mode) -> Result<String> {
    let raw_target = match program {
        Program::Local { dest, .. } => dest,
        Program::Sd(path) => path,
    };
    if mode == Mode::Applet && launch::is_game(raw_target, &crate::project::load_all().unwrap_or_default()) {
        bail!("{raw_target} is a game: it runs with full memory only (mode \"full\"), never in applet mode");
    }
    match program {
        Program::Local { dest, .. } => {
            let dest = check_path(dest, true)?;
            if !dest.starts_with("/switch/") {
                bail!("NRO destination must be under /switch/ (got {dest})");
            }
            if !dest.to_ascii_lowercase().ends_with(".nro") {
                bail!("{dest} isn't an .nro path");
            }
            Ok(dest)
        }
        Program::Sd(path) => {
            let path = check_path(path, false)?;
            if !path.to_ascii_lowercase().ends_with(".nro") || client.stat(&path).await.is_err() {
                bail!("{path} isn't an NRO on the SD card");
            }
            Ok(path)
        }
    }
}

/// Uploads a local NRO to `sd_dest` (over USB if requested and available, otherwise over Wi-Fi) while the
/// current NRO is still running, or stats an existing SD card NRO.
async fn stage_nro_on_sd(client: &Client, program: &Program, sd_dest: &str, use_usb: bool) -> Result<String> {
    match program {
        Program::Local { nro, .. } => {
            if use_usb {
                match upload_bytes_via_usb(client, nro, sd_dest).await {
                    Ok(up) => {
                        return Ok(format!(
                            "uploaded {} bytes to {sd_dest} via {} in {:.2} s",
                            up.size,
                            up.via,
                            up.elapsed.as_secs_f64()
                        ));
                    }
                    Err(usb_err) => {
                        let stats = client
                            .upload(sd_dest, nro)
                            .await
                            .with_context(|| format!("uploading {sd_dest} over Wi-Fi after USB failed ({usb_err:#})"))?;
                        return Ok(format!(
                            "uploaded {} bytes to {sd_dest} over Wi-Fi in {:.2} s (USB unavailable: {usb_err})",
                            stats.size,
                            stats.elapsed.as_secs_f64()
                        ));
                    }
                }
            }
            let stats = client.upload(sd_dest, nro).await.with_context(|| format!("uploading {sd_dest}"))?;
            Ok(format!(
                "uploaded {} bytes to {sd_dest} over Wi-Fi in {:.2} s",
                stats.size,
                stats.elapsed.as_secs_f64()
            ))
        }
        Program::Sd(_) => {
            let entry = client.stat(sd_dest).await.with_context(|| format!("checking {sd_dest} on the SD card"))?;
            Ok(format!("using {sd_dest} already on the SD card ({} bytes)", entry.size))
        }
    }
}

/// Streams `nro` bytes through [`crate::usb::upload`] using a temporary file.
async fn upload_bytes_via_usb(client: &Client, nro: &[u8], sd_dest: &str) -> Result<crate::usb::UsbUpload> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("kickstand-reload-{}-{id}.nro", std::process::id()));
    tokio::fs::write(&tmp, nro).await.with_context(|| format!("writing temporary NRO {}", tmp.display()))?;
    let res = crate::usb::upload(client, &tmp, sd_dest).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    res
}

enum HotOutcome {
    Hot(Started),
    Cold { reason: String },
}

/// Attempts the fast in-process reload when `hbloader` is already running in `mode`.
async fn try_in_process_reload(
    client: &Client,
    sd_dest: &str,
    args: &[String],
    mode: Mode,
    exit_buttons: Option<&[Button]>,
) -> Result<HotOutcome> {
    let Some(app) = launch::status(client, mode).await? else {
        return Ok(HotOutcome::Cold { reason: "no application was running".into() });
    };
    if app.hbl != Some(true) {
        return Ok(HotOutcome::Cold {
            reason: format!("title {:016x} isn't running under hbloader", app.program_id),
        });
    }

    let (launcher, _) = match client.read(launch::LAUNCHER, 0, u64::MAX).await {
        Ok(read) => read,
        Err(e) => {
            return Ok(HotOutcome::Cold {
                reason: format!("couldn't read {} ({e})", launch::LAUNCHER),
            });
        }
    };

    let mut notes = Vec::new();
    let buttons = default_exit_buttons(exit_buttons);
    let already_at_hbmenu = app.pid != 0 && app.pid == launch::HBMENU_PID.load(Ordering::Relaxed) && exit_buttons.is_none();

    if !already_at_hbmenu && !buttons.is_empty() {
        let names: Vec<&str> = buttons.iter().map(|b| b.name()).collect();
        client.press(buttons.clone(), 1, 120).await?;
        notes.push(format!(
            "pressed {} to exit the running NRO back to hbmenu in-process (pid {})",
            names.join("+"),
            app.pid
        ));
        tokio::time::sleep(EXIT_SETTLE).await;
        match launch::status(client, mode).await? {
            Some(now) if now.hbl == Some(true) => {}
            _ => {
                return Ok(HotOutcome::Cold {
                    reason: "hbloader exited to the Home Menu instead of returning to hbmenu".into(),
                });
            }
        }
    } else if already_at_hbmenu {
        notes.push(format!("hbmenu was already open in hbloader (pid {})", app.pid));
    }

    let mut sock = launch::start_netloader(client, HOT_NETLOADER_WAIT).await.ok();
    if sock.is_none() && already_at_hbmenu && !buttons.is_empty() {
        // HBMENU_PID was stale and an NRO was actually running; press the exit button(s) and try once more.
        let names: Vec<&str> = buttons.iter().map(|b| b.name()).collect();
        client.press(buttons, 1, 120).await?;
        notes.push(format!(
            "hbmenu didn't answer right away; pressed {} to exit the running NRO to hbmenu",
            names.join("+")
        ));
        tokio::time::sleep(EXIT_SETTLE).await;
        sock = launch::start_netloader(client, HOT_NETLOADER_WAIT).await.ok();
    }

    let Some(sock) = sock else {
        return Ok(HotOutcome::Cold {
            reason: format!(
                "hbmenu's NetLoader didn't answer within {:.1} s after in-process exit",
                HOT_NETLOADER_WAIT.as_secs_f64()
            ),
        });
    };

    let output = nxlink::OutputListener::bind().await?;
    launch::HBMENU_PID.store(0, Ordering::Relaxed);
    let launcher_dest = launch::LAUNCHER.trim_start_matches("/switch/").to_string();
    let chain_args = chainload_args(sd_dest, args);
    nxlink::send(sock, &launcher, &launcher_dest, &chain_args).await?;
    notes.push(format!(
        "chainloaded {sd_dest} in-process through {} (same hbloader pid {})",
        launch::LAUNCHER,
        app.pid
    ));

    let pid = launch::status(client, mode).await?.map_or(app.pid, |a| a.pid);
    let output = output.accept(HOT_OUTPUT_WAIT).await?;
    if output.is_none() {
        notes.push("the NRO didn't connect its stdout (it may not use nxlink stdio)".into());
    }

    Ok(HotOutcome::Hot(Started { notes, pid, output }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::mock;

    #[test]
    fn builds_chainload_args_and_default_exit_buttons() {
        assert_eq!(
            chainload_args("/switch/mygame/mygame.nro", &["--level".into(), "3".into()]),
            vec!["--open", "/switch/mygame/mygame.nro", "--level", "3"]
        );
        assert_eq!(default_exit_buttons(None), vec![Button::Plus]);
        assert_eq!(
            default_exit_buttons(Some(&[Button::Minus, Button::Plus])),
            vec![Button::Minus, Button::Plus]
        );
        assert!(default_exit_buttons(Some(&[])).is_empty());
    }

    #[tokio::test]
    async fn refuses_destinations_outside_switch_or_non_nro() {
        let root = std::env::temp_dir().join(format!("kickstand-reload-val-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(mock::serve_console(listener, Arc::new(mock::Console::new(root.clone(), vec!["tok".into()], None))));
        let client = Client::new(addr, "tok".into());

        let bad_dir = hot_reload(
            &client,
            ReloadOptions {
                program: Program::Local { nro: b"NRO0".to_vec(), dest: "/kickstand/test.nro".into() },
                args: Vec::new(),
                mode: Mode::Full,
                use_usb: false,
                exit_buttons: None,
                replay_macro: None,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(bad_dir.contains("/switch/"), "{bad_dir}");

        let bad_ext = hot_reload(
            &client,
            ReloadOptions {
                program: Program::Local { nro: b"NRO0".to_vec(), dest: "/switch/mygame/mygame.bin".into() },
                args: Vec::new(),
                mode: Mode::Full,
                use_usb: false,
                exit_buttons: None,
                replay_macro: None,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(bad_ext.contains(".nro"), "{bad_ext}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn stages_nro_over_wifi_and_mounted_sd() {
        let root = std::env::temp_dir().join(format!("kickstand-reload-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("atmosphere")).unwrap();
        std::fs::create_dir_all(root.join("switch")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(mock::serve_console(listener, Arc::new(mock::Console::new(root.clone(), vec!["tok".into()], None))));
        let client = Client::new(addr, "tok".into());

        // 1. Wi-Fi upload (`use_usb: false`)
        let payload = b"NRO0-wifi-payload".to_vec();
        let note = stage_nro_on_sd(
            &client,
            &Program::Local { nro: payload.clone(), dest: "/switch/mygame/mygame.nro".into() },
            "/switch/mygame/mygame.nro",
            false,
        )
        .await
        .unwrap();
        assert!(note.contains("over Wi-Fi"), "{note}");
        assert_eq!(std::fs::read(root.join("switch/mygame/mygame.nro")).unwrap(), payload);

        // 2. USB requested when no USB cable/mount exists -> seamlessly falls back to Wi-Fi
        let payload2 = b"NRO0-usb-fallback-payload".to_vec();
        let note2 = stage_nro_on_sd(
            &client,
            &Program::Local { nro: payload2.clone(), dest: "/switch/mygame/mygame.nro".into() },
            "/switch/mygame/mygame.nro",
            true,
        )
        .await
        .unwrap();
        assert!(note2.contains("USB unavailable") || note2.contains("via"), "{note2}");
        assert_eq!(std::fs::read(root.join("switch/mygame/mygame.nro")).unwrap(), payload2);

        // 3. Existing SD card NRO (`Program::Sd`)
        let note3 = stage_nro_on_sd(&client, &Program::Sd("/switch/mygame/mygame.nro".into()), "/switch/mygame/mygame.nro", false)
            .await
            .unwrap();
        assert!(note3.contains("already on the SD card"), "{note3}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn hot_reloads_in_process_via_launcher_when_netloader_answers() {
        // Bind NetLoader port 28280 on 127.0.0.1 if free; skip only if another process holds 28280.
        let Ok(netloader) = tokio::net::TcpListener::bind(("127.0.0.1", nxlink::SERVER_PORT)).await else {
            return;
        };

        let root = std::env::temp_dir().join(format!("kickstand-reload-hot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("switch/kickstand")).unwrap();
        let launcher_bytes = b"KICKSTAND-LAUNCHER-NRO".to_vec();
        std::fs::write(root.join("switch/kickstand/kickstand.nro"), &launcher_bytes).unwrap();

        let console = Arc::new(mock::Console::new(root.clone(), vec!["tok".into()], None));
        *console.app.lock().unwrap() = mock::MockApp {
            running: Some(proto::App { pid: 0x42, program_id: 0x0100_0000_0000_1000, hbl: Some(true) }),
            ignores_exit: false,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(mock::serve_console(listener, console));
        let client = Client::new(addr, "tok".into());

        // Fake hbmenu NetLoader receiving `kickstand.nro --open /switch/mygame/mygame.nro --fast`
        let netloader_task = tokio::spawn(async move {
            let (mut s, _) = netloader.accept().await.unwrap();
            let name_len = s.read_u32_le().await.unwrap() as usize;
            let mut name = vec![0; name_len];
            s.read_exact(&mut name).await.unwrap();
            let size = s.read_u32_le().await.unwrap() as usize;
            s.write_i32_le(0).await.unwrap();
            let mut inflate = flate2::Decompress::new(true);
            let mut out = Vec::with_capacity(size);
            while out.len() < size {
                let len = s.read_u32_le().await.unwrap() as usize;
                let mut piece = vec![0; len];
                s.read_exact(&mut piece).await.unwrap();
                inflate.decompress_vec(&piece, &mut out, flate2::FlushDecompress::None).unwrap();
            }
            s.write_i32_le(0).await.unwrap();
            let argv_len = s.read_u32_le().await.unwrap() as usize;
            let mut argv = vec![0; argv_len];
            s.read_exact(&mut argv).await.unwrap();
            (String::from_utf8(name).unwrap(), out, argv)
        });

        let game_nro = b"NEW-GAME-BUILD-NRO".to_vec();
        let reloaded = hot_reload(
            &client,
            ReloadOptions {
                program: Program::Local { nro: game_nro.clone(), dest: "/switch/mygame/mygame.nro".into() },
                args: vec!["--fast".into()],
                mode: Mode::Full,
                use_usb: false,
                exit_buttons: None,
                replay_macro: None,
            },
        )
        .await
        .unwrap();

        assert!(reloaded.hot, "expected hot in-process reload, got notes: {:?}", reloaded.started.notes);
        assert_eq!(reloaded.started.pid, 0x42);
        assert!(reloaded.elapsed < Duration::from_secs(5));
        assert!(reloaded.transfer_note.contains("/switch/mygame/mygame.nro"), "{}", reloaded.transfer_note);
        assert_eq!(std::fs::read(root.join("switch/mygame/mygame.nro")).unwrap(), game_nro);

        let (sent_name, sent_nro, sent_argv) = netloader_task.await.unwrap();
        assert_eq!(sent_name, "kickstand/kickstand.nro");
        assert_eq!(sent_nro, launcher_bytes);
        assert_eq!(sent_argv, b"--open\0/switch/mygame/mygame.nro\0--fast\0");

        let _ = std::fs::remove_dir_all(&root);
    }
}
