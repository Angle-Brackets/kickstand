<p align="center">
  <img src="assets/logo/kickstand-swing.svg" width="112" alt="Kickstand logo: a K whose red lower leg is the Switch kickstand, kicking open">
</p>
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/kickstand-wordmark-dark.svg">
    <img src="assets/logo/kickstand-wordmark-light.svg" width="360" alt="kickstand">
  </picture>
</p>
<p align="center"><b>Deploy, run, and debug homebrew with AI</b></p>

Lets an AI agent develop and test homebrew on a real Nintendo Switch (Atmosphère) with nobody holding it:
send a build and run it with full memory, read its stdout, look at the screen, play it with buttons, sticks
and touch, read and poke its memory, read crash reports with symbolized backtraces, and recover the
console with a warm reboot into hekate.

All Rust:
- `daemon/`: runs on the Switch. Builds as a sysmodule (`daemon/sysmodule`, the normal way) and as an
  hbmenu NRO (`daemon/app`), which also has a launcher mode the MCP uses. Nightly, built with `cargo nx`.
- `mcp/`: the MCP server on the PC (stdio, on `rmcp`), plus a CLI with the same features. Stable Rust.
- `proto/`: the wire protocol both sides share (no_std + serde/postcard).

Status: in daily use on hardware (fw 23.0.1, Atmosphère) for the PaperBoat port. See `context/DESIGN.md`
and `context/AGENT_LOG.md`.

## Build
```sh
cargo build --release                                    # PC side: target/release/kickstand
cd daemon && cargo nx build --release -p sys-kickstand   # sysmodule NSP (needs cargo-nx, see AGENT_LOG)
cd daemon && cargo nx build --release -p kickstand-nro   # kickstand.nro (first install, launcher)
```

## Set up
```sh
target/release/kickstand token                  # create the shared token (~/.config/kickstand/token)
echo 192.168.x.y > ~/.config/kickstand/host     # or pass --host / set SWITCH_IP
scripts/run-daemon-nro.sh                        # first time: start the NRO daemon through hbmenu's NetLoader
target/release/kickstand sysmodule install daemon/target/aarch64-nintendo-switch-freestanding/release/sys-kickstand.nsp
target/release/kickstand sysmodule handover      # start the sysmodule; the NRO exits
target/release/kickstand put daemon/target/aarch64-nintendo-switch-freestanding/release/kickstand.nro /switch/kickstand/kickstand.nro
claude mcp add kickstand -- "$PWD/target/release/kickstand" serve
```
Starting the sysmodule at boot (`boot2.flag`) is left to the user (`kickstand sysmodule enable`); the MCP
tools never set it.

## MCP tools
| Area | Tools |
| - | - |
| Run homebrew | `run_nro` (send a local build or pick one on the SD card; opens hbmenu with full memory by starting a game with R held, starts NetLoader, streams stdout), `nro_output`, `open_hbmenu`, `app_status`, `close_app` |
| Games | `list_apps`, `launch_title` |
| See | `screenshot` (scaled or cropped to save tokens), `record_gif` (returns a contact sheet) |
| Play | `press`, `input` (timed steps: buttons held across steps, analog sticks, touch) |
| Debug | `crash_reports`, `crash_report` (Atmosphère reports summarized, fatal records decoded, the crash screenshot), `symbolize` (addr2line on the PC; `run_nro` output is symbolized live with `elf`), `memory_map`, `read_memory`, `write_memory` (address expressions with ELF symbols and pointer chains; writes to homebrew only) |
| Files | `list_dir`, `stat`, `read_file`, `download`, `upload`, `delete` |
| Console | `ping`, `resources`, `keep_awake`, `sysmodule`, `hekate_entries`, `reboot` (warm reboot into hekate: an entry, UMS or the menu; needs `confirm`) |

The CLI has the same features (`kickstand --help`), plus `kickstand watch`: a page showing the screen
live and what's running, so a person can follow the agent (`--bind 0.0.0.0` for a phone on the network).

Safety: writes and deletes only in `/switch/paperboat/`, `/switch/ttydrecomp/`, `/switch/kickstand/`,
`/kickstand/` and the sysmodule's own folder; keys, NAND backups and `/Nintendo` are never read; memory
writes only to homebrew (hbloader), never a game; reboots are warm reboots to hekate only (RCM-injected
consoles lose hekate on a cold boot). Every request keeps the console awake for 30 minutes (a Kickstand
icon by the clock shows it), then its own sleep timers apply again.

## License
GPL-3.0-only (see `LICENSE`). `third_party/nx` is MIT.
