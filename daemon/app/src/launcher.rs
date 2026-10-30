// SPDX-License-Identifier: GPL-3.0-only
//! Launcher mode: the NRO's arguments ask it to start something else, which it does without any UI, then
//! exits. This is how the agent opens things from inside the hbl app session (with full memory), where
//! the sysmodule can't:
//!
//! - `--open <sd path> [args...]`: hbloader loads that NRO next (`envSetNextLoad`). If this NRO came through
//!   nxlink, the PC's `<addr>_NXLINK_` marker is passed on, so the target's stdout reaches the same PC.
//! - `--title <hex id>`: am closes this application and starts the installed title
//!   (`appletRequestLaunchApplication`).
//!
//! Errors go to the daemon log (`/kickstand/logs/daemon.log`), prefixed `launcher:`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use daemon_core::log;
use nx::applet::{self, AppletProxy};
use nx::ipc::sf::applet::{IApplicationFunctionsClient, IApplicationProxyClient};
use nx::result::Result;

pub enum Launch {
    Open { path: String, args: Vec<&'static str> },
    Title(u64),
}

/// What the arguments ask for: `None` to run as the daemon, `Some(Err)` for a malformed request.
pub fn requested() -> Option<core::result::Result<Launch, String>> {
    let mut args = nxlink::args();
    let request = match args.next()? {
        "--open" => match args.next() {
            Some(path) => sd_path(path).map(|path| Launch::Open { path, args: args.collect() }),
            None => Err(String::from("--open needs a path")),
        },
        "--title" => match args.next().map(|id| u64::from_str_radix(id.trim_start_matches("0x"), 16)) {
            Some(Ok(id)) if id != 0 => Ok(Launch::Title(id)),
            _ => Err(String::from("--title needs a hex title id")),
        },
        _ => return None,
    };
    Some(request)
}

/// `/switch/x.nro` or `sdmc:/switch/x.nro` → `sdmc:/switch/x.nro`, for paths with an `.nro` extension.
fn sd_path(path: &str) -> core::result::Result<String, String> {
    let rest = path.strip_prefix("sdmc:").unwrap_or(path);
    if !rest.starts_with('/') || !rest.to_ascii_lowercase().ends_with(".nro") {
        return Err(format!("not an absolute .nro path on the SD card: {path}"));
    }
    Ok(format!("sdmc:{rest}"))
}

/// Carries out `launch`. Returns once the next program is set up; the caller should then exit.
pub fn run(launch: Launch) -> Result<()> {
    match launch {
        Launch::Open { path, args } => {
            // hbmenu's argv convention: every argument quoted, the program path first.
            let mut argv = format!("\"{path}\"");
            for arg in args.iter().copied().chain(nxlink::host_arg()) {
                argv.push_str(&format!(" \"{arg}\""));
            }
            log!("launcher: open {argv}");
            // hbloader reads these after we exit; they must outlive main.
            let (path, argv) = (String::leak(path), String::leak(argv));
            let (path_set, argv_set) = nx::hbl::set_next_load_entry(path, argv)?;
            if !(path_set && argv_set) {
                log!("launcher: not running under hbloader, nothing to chain-load into");
            }
            Ok(())
        }
        Launch::Title(id) => {
            log!("launcher: start title {id:016x}");
            match applet::get_applet_proxy().as_ref() {
                Some(AppletProxy::Application(proxy)) => {
                    proxy.get_application_functions()?.create_application_and_request_to_start(id)
                }
                _ => {
                    log!("launcher: not running as an application, can't start a title");
                    Ok(())
                }
            }
        }
    }
}
