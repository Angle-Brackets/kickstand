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
<p align="center">
  <a href="https://github.com/Angle-Brackets/kickstand/actions/workflows/ci.yml">
    <img src="https://github.com/Angle-Brackets/kickstand/actions/workflows/ci.yml/badge.svg" alt="tests">
  </a>
</p>

**Kickstand** lets AI agents and developers autonomously build, deploy, run, play, and debug homebrew on a real Nintendo Switch running Atmosphère — with nobody holding the console.

> **Full Tool & CLI Reference:** See [**`TOOLS.md`**](TOOLS.md) for complete documentation of all 45 MCP tools, their parameters, and matching CLI commands (every CLI command takes `--json` for scripts).

---

## Core Capabilities

- **One-Call Build & Test (`test_build`):** Compiles a project locally, verifies the NRO matches its debug ELF, uploads it to the SD card, launches it with full title-takeover memory, and supervises execution — returning stdout, outcome, and a screenshot.
- **Full-Memory Homebrew Execution (`run_nro`):** Uploads and launches NROs via `nxlink`/NetLoader using Atmosphère's host-title override for full RAM access by default (no physical button hold required), or runs lightweight utilities in `applet` mode while streaming live `stdout`/`stderr`.
- **Screen Vision & Visual Waiting (`screenshot`, `wait_screen`, `record_gif`):** Captures JPEG screenshots (downscaled or cropped to save context tokens, with optional 100px coordinate grids), records GIFs with contact sheets, and blocks until the screen changes, settles, or matches a visual template from an earlier frame.
- **Controller & Touch Automation (`press`, `input`, `input_until`):** Simulates controller buttons, dual analog sticks, and touch gestures across timed steps, or holds/mashes/replays input while watching the screen until a visual condition triggers (e.g., *"hold right until the doorway appears"*).
- **Macros (`record_macro`, `play_macro`, `control_macro`, `type_text`):** Record controller input once while you play (from the desktop app's Macros page, `kickstand macro record`, or an agent asking you) and let agents replay it to get back to the same spot in a game before each test. Pause, resume or stop a macro while it plays, from the desktop app, `kickstand macro pause|resume|stop` (or Ctrl-C), or an agent. Type into the software keyboard on a virtual USB keyboard.
- **Live Debugging & Crash Analysis (`threads`, `symbolize`, `crash_report`, `read_memory`, `write_memory`):** Symbolizes live output and thread backtraces with source file/line numbers via `addr2line`, decodes Atmosphère crash dumps and fatal error records, inspects process memory maps, and evaluates symbolic pointer chains (`main+0x1234`, `[sym+0x10]+0x8`).
- **SD Card File & Log Management (`list_dir`, `read_file`, `tail_file`, `find`, `grep`, `upload`, `download`, `delete`):** Transfers files, tails growing log files, and finds or greps files on the console itself, on the SD card with interactive folder permission prompts and system path guardrails.
- **Self-Updating Daemon & Recovery (`status`, `sysmodule`, `reboot`, `keep_awake`):** Hot-reloads the background sysmodule in ~11 seconds without rebooting, monitors system RAM/temp/battery, keeps the console awake during active sessions, and performs warm reboots into Hekate entries or USB Mass Storage (UMS).

---

## Architecture

Kickstand is written 100% in Rust across three workspace components:

| Component | Description |
| :--- | :--- |
| **`daemon/`** | Runs on the Nintendo Switch (nightly Rust, built with `cargo nx`). Builds both as a background service (**`daemon/sysmodule`**, the standard operating mode) and as an hbmenu application (**`daemon/app`**, `kickstand.nro`). Opened from hbmenu, the NRO displays live status (connected agent activity, IP address, pairing token, background service state, and logs; `+` exits and hands back to the sysmodule) and also serves as the launcher helper used by the MCP server. |
| **`mcp/`** | Runs on the PC (stable Rust). A single binary (`kickstand`) that acts as the **egui desktop application** (when run with no arguments), the **stdio MCP server** (`kickstand serve`, built on `rmcp`), and a **full-featured CLI** (`kickstand <subcommand>`). |
| **`proto/`** | Shared wire protocol crate (`no_std` + `serde`/`postcard`) used by both the Switch daemon and PC client. |

> **Hardware Status:** In daily production use on hardware (Horizon OS 23.0.1, Atmosphère) for the PaperBoat port. See [`context/DESIGN.md`](context/DESIGN.md) for design notes.

---

## Building from Source

```sh
# 1. Build the PC binary (desktop GUI, MCP server, and CLI)
cargo build --release                                    # -> target/release/kickstand

# 2. Build the Switch background sysmodule NSP (requires cargo-nx)
cd daemon && cargo nx build --release -p sys-kickstand   # -> daemon/target/aarch64-nintendo-switch-freestanding/release/sys-kickstand.nsp

# 3. Build the Switch hbmenu NRO (first-time installer, status screen & launcher)
cd daemon && cargo nx build --release -p kickstand-nro   # -> daemon/target/aarch64-nintendo-switch-freestanding/release/kickstand.nro
```

---

## Setup & Pairing

### Option A: Desktop GUI (Recommended)

1. Copy `kickstand.nro` to `/switch/kickstand/kickstand.nro` on your Switch's SD card and open it from **hbmenu**. The screen will display a 6-digit pairing code.
2. On a PC connected to the same Wi-Fi network, run `kickstand` with no arguments to launch the desktop app.
3. The app automatically discovers the Switch, accepts the pairing code, and links your AI client with one click (the background sysmodule is installed from the command line, below):
   - **Claude:** Claude Code (`code`), Claude Desktop (`desktop`)
   - **Gemini:** Gemini CLI (`gemini`), Antigravity CLI (`agy`), Antigravity IDE (`antigravity`)
   - **OpenAI:** Codex (`codex`)
   - **Custom:** LM Studio (`lmstudio`), plus ready-to-copy command and JSON snippets for any other MCP client.
4. Once linked, the desktop window shows a live feed of connected agents and their actions on the console.

To unpair, click **Unpair** beside *This Switch* in the desktop app, or run `kickstand unpair`: the Switch forgets this PC's token (other paired PCs keep working). You can also delete the PC's line from `/kickstand/token.txt` on the SD card; the daemon reads that file again on every connection, so either way takes effect at once.

Discovery uses UDP port 28791 (broadcast) and the daemon listens on TCP port 28790. Both are fixed, so any PC and Switch running Kickstand find each other without setup; allow them through a firewall on the PC if discovery comes up empty.

### Updating

The desktop app looks for a new release when it opens and every 6 hours, and shows **Update to X** in its footer; one click downloads it, checks its signature and reopens. When the Switch is then behind the PC, **Update** appears beside *This Switch*: the PC puts its own version's sysmodule and NRO on the Switch over the usual connection (the Switch never goes online itself), restarting the background service in about 15 seconds without a reboot. It waits until nothing is running on the Switch and no agent is at work. From a terminal: `kickstand update` (both), `kickstand update --check`, `kickstand update --switch`. Agents can't update anything.

Releases are only installed if their `SHA256SUMS` is signed with Kickstand's release key, and every file matches it. A copy built from source updates with `git pull` and `cargo build` instead.

To add Kickstand to your OS application launcher (Linux & macOS):
```sh
kickstand install            # Install desktop entry and icon (--remove to uninstall)
```

### Option B: Command Line Setup

```sh
# Discover Switches running Kickstand on the local network and pair using the 6-digit code
target/release/kickstand discover
target/release/kickstand pair 482913

# Install the background sysmodule and hand over from the NRO
target/release/kickstand sysmodule install daemon/target/aarch64-nintendo-switch-freestanding/release/sys-kickstand.nsp
target/release/kickstand sysmodule handover

# Keep kickstand.nro on the SD card (used as the launcher helper and status screen)
target/release/kickstand put daemon/target/aarch64-nintendo-switch-freestanding/release/kickstand.nro /switch/kickstand/kickstand.nro

# Register the MCP server with your AI client (code, desktop, gemini, agy, antigravity, codex, lmstudio)
target/release/kickstand link code
```

> **Note on Boot Autostart:** Enabling the sysmodule at console boot (`boot2.flag`) is an explicit user choice via `kickstand sysmodule enable` (or `disable`). MCP tools exposed to AI agents cannot toggle boot autostart.

### Multi-Agent Tracking & Live Browser View

- **Agent Attribution (`--app`):** `kickstand link` configures `kickstand serve --app <app>`, allowing the console and desktop app to count and tag each connected agent (`[Claude Agent #1]`, `[Antigravity Agent #1]`). Multiple agents can connect simultaneously; `status` reports other active agents, and state-changing tools warn if another agent recently interacted with the console.
- **Live Browser Stream (`kickstand watch`):** Starts a local web page showing the Switch screen and running process live so you can watch an agent work (pass `--bind 0.0.0.0` to view from a phone on the same network).

---

## Agent Skill (Optional)

[`skills/kickstand/SKILL.md`](skills/kickstand/SKILL.md) gives agents the operating rules for a real console: check `status` first, keep it awake (and let it sleep when done), wait on the screen instead of sleeping, replay macros to get back to a spot, and ask before deleting, writing memory or rebooting.

- **Claude Code:** copy the `skills/kickstand` folder to `~/.claude/skills/` (every project) or to `.claude/skills/` in your project. Claude loads it whenever the task touches the Switch.
- **Other AI apps:** paste the file's contents (everything after the `---` header) into the app's custom instructions or rules for the project.

---

## Project Configuration (`projects.json`)

To let an agent build, deploy, and test a project in a single `test_build` call, configure `~/.config/kickstand/projects.json`:

```json
{
  "paperboat": {
    "dir": "/path/to/project",
    "build": "cargo nx build --release",
    "nro": "target/aarch64-nintendo-switch-freestanding/release/paperboat.nro",
    "elf": "target/aarch64-nintendo-switch-freestanding/release/paperboat.elf",
    "dest": "/switch/paperboat/paperboat.nro",
    "args": ["--skip-intro", "--log=sdmc:/paperboat.log"]
  }
}
```

- `build` (optional) runs in `dir`; `nro` and `elf` are relative to it. `module` (optional) names the module in its backtraces (default: the ELF's file name).
- `dest` is where the NRO goes on the SD card (its folder must exist); that folder is open to the agent without asking.
- `args` (optional) is the command line the NRO runs with, so an agent doesn't retype (or drift from) a long one; `test_build`'s own `args` replace it for one run.

Use the `projects` MCP tool (or `kickstand projects`) to inspect configured projects and verify their paths.

---

## MCP Tools Overview

Kickstand provides **45 MCP tools** across 6 categories. For full parameter tables, syntax guides, and CLI equivalents, see [**`TOOLS.md`**](TOOLS.md).

| Category | Tools | Highlights |
| :--- | :--- | :--- |
| **[Develop & Run](TOOLS.md#1-develop--run-homebrew)** | `test_build`, `projects`, `run_nro`, `nro_output`, `supervise`, `open_hbmenu`, `quit_app`, `force_quit_app`, `list_apps`, `launch_title` | Build, verify NRO/ELF match, deploy, launch with full memory via Atmosphère title takeover (or `applet` mode), stream stdout, and supervise until exit, crash, or hang. |
| **[See (Screen Vision)](TOOLS.md#2-screen-vision--capture)** | `screenshot`, `wait_screen`, `record_gif` | Downscaled/cropped screenshots to save tokens, 100px coordinate overlays (`grid`), shot numbering, GIF contact sheets, and visual condition waiting. |
| **[Play (Input)](TOOLS.md#3-controller-keyboard--macros)** | `press`, `input`, `input_until`, `type_text`, `macros`, `record_macro`, `play_macro`, `control_macro` | Single presses, multi-step button/stick/touch sequences, closed-loop input (`input_until`) that holds, mashes, or replays actions until a screen region changes, settles, or matches an earlier shot, typing on a virtual keyboard, and macros: controller input you record once (in the desktop app or `kickstand macro record`) that agents replay to get back to the same place in a game, and pause, resume or stop while it plays. |
| **[Debug & Memory](TOOLS.md#4-debugging-crash-analysis--memory)** | `crash_reports`, `crash_report`, `symbolize`, `threads`, `memory_map`, `read_memory`, `write_memory` | Decoded Atmosphère crash reports, live `addr2line` symbolization, all-thread stack traces with source lines, and symbolic memory reads/writes (`main+0x1234`, `[sym+0x10]+0x8`). |
| **[SD Card Files](TOOLS.md#5-sd-card-files--logs)** | `list_dir`, `stat`, `read_file`, `tail_file`, `find`, `grep`, `upload`, `download`, `delete`, `answer_access` | Browse, inspect, upload, download, delete, live-tail SD card logs (`tail_file` can block until a target string appears), and find files or grep them on the console without downloading. |
| **[Console & Daemon](TOOLS.md#6-console--daemon-management)** | `status`, `ping`, `resources`, `keep_awake`, `sysmodule`, `hekate_entries`, `reboot` | Check running app/fatal/lock-screen state, monitor RAM/SoC temp/battery, hot-swap the sysmodule in ~11s, and warm-reboot into Hekate entries or UMS. |

---

## Safety & Guardrails

Kickstand is designed so an AI agent can iterate autonomously without risking console state, keys, or personal data:

- **SD Card Folder Permissions:** Agents can freely access Kickstand's own directories, `/atmosphere/crash_reports`, deployment folders configured in `projects.json`, and (for reading) the `sdmc:/` paths an NRO is started with, such as its log file. Accessing any other SD card folder triggers an interactive permission prompt in the AI client (*"Just this once"*, *"Until Kickstand restarts"*, or *"From now on"*; a file at the card's root is asked about by itself, not the whole card), regardless of the client's auto-approve mode.
  - If the AI app can't show that prompt, Kickstand asks in its own desktop window instead (opening it if it's closed) with the same choices, and the agent asks you in the chat at the same time, so you can answer from wherever you are (say, Claude on your phone): whichever you answer counts, and an answer in the chat takes the question out of the window.
  - You can pre-authorize folders from a terminal with `kickstand allow FOLDER [--write]` (saved in `~/.config/kickstand/access.json`).
  - Interactive CLI commands run by a human in a terminal work anywhere; scripted or agent-invoked CLI runs enforce the same allowlist.
- **Hard Console Path Blocks:** The Switch daemon itself refuses to write to Atmosphère, Hekate, or Nintendo system directories (except updating Kickstand's own sysmodule), the SD card root, or key files, and never reads keys, NAND backups, or `/Nintendo`.
- **Memory & Process Safety:**
  - Projects configured in `projects.json` always launch with full memory (never applet mode).
  - `write_memory` only writes to homebrew processes (`hbloader`), never retail games or system processes, and warns if the local ELF build ID differs from the running binary.
  - `quit_app` closes programs the way a person does (HOME, then Close software), so they can save and flush output first; terminating one at once is a separate tool, `force_quit_app`, for hangs. The Album is always closed with `HOME`, never terminated.
- **RCM-Safe Reboots:** `reboot` only performs warm reboots into Hekate (a boot entry, UMS, or the Hekate menu) and requires `confirm: true` — preventing cold boots that would lose RCM payload injection on unpatched consoles.
- **Automatic Sleep Prevention:** Every request extends a 30-minute keep-awake lease on the console (indicated by a Kickstand icon next to the system clock), after which normal Switch sleep timers resume.

---

## License

- **Kickstand:** `GPL-3.0-only` (see [`LICENSE`](LICENSE)).
- **`third_party/nx`:** MIT License.
- **Fonts (`assets/fonts/`):** Archivo and JetBrains Mono are licensed under the SIL Open Font License 1.1 (see the license files alongside them).

## Releasing

1. `scripts/bump-version.sh 0.1.0` sets the PC and Switch versions (committing if they change) and tags `v0.1.0`.
2. `git push && git push origin v0.1.0`. The `release` workflow builds the PC app (Linux x86_64, macOS arm64 and x86_64), the sysmodule and the NRO, signs `SHA256SUMS` with the `KICKSTAND_SIGNING_KEY` secret and publishes the release; installed copies offer it from then on.

The key was made once with `cargo run -p kickstand-release -- keygen` (its public half is `PUBLIC_KEY` in `mcp/src/update.rs`) and stored with `gh secret set KICKSTAND_SIGNING_KEY < ~/.config/kickstand-release/signing.key`. Keep that file backed up: GitHub never shows a secret again, and without the key installed copies can't take new releases.
