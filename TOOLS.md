# Kickstand Capabilities & MCP Tools Reference

This document details all **43 MCP tools** exposed by `kickstand serve` and the matching **CLI commands** provided by the `kickstand` binary.

## At a Glance

The tool names are the same in every AI app.

| Area | Tools |
| :--- | :--- |
| [Develop & Run Homebrew](#1-develop--run-homebrew) | [`test_build`](#test_build) · [`projects`](#projects) · [`run_nro`](#run_nro) · [`nro_output`](#nro_output) · [`supervise`](#supervise) · [`open_hbmenu`](#open_hbmenu) · [`close_app`](#close_app) · [`list_apps`](#list_apps) · [`launch_title`](#launch_title) |
| [Screen Vision & Capture](#2-screen-vision--capture) | [`screenshot`](#screenshot) · [`wait_screen`](#wait_screen) · [`record_gif`](#record_gif) |
| [Controller, Keyboard & Macros](#3-controller-keyboard--macros) | [`press`](#press) · [`input`](#input) · [`input_until`](#input_until) · [`type_text`](#type_text) · [`macros`](#macros) · [`record_macro`](#record_macro) · [`play_macro`](#play_macro) |
| [Debugging, Crash Analysis & Memory](#4-debugging-crash-analysis--memory) | [`crash_reports`](#crash_reports) · [`crash_report`](#crash_report) · [`symbolize`](#symbolize) · [`threads`](#threads) · [`memory_map`](#memory_map) · [`read_memory`](#read_memory) · [`write_memory`](#write_memory) |
| [SD Card Files & Logs](#5-sd-card-files--logs) | [`list_dir`](#list_dir) · [`stat`](#stat) · [`read_file`](#read_file) · [`tail_file`](#tail_file) · [`find`](#find) · [`grep`](#grep) · [`upload`](#upload) · [`download`](#download) · [`delete`](#delete) · [`answer_access`](#answer_access) |
| [Console & Daemon Management](#6-console--daemon-management) | [`status`](#status) · [`ping`](#ping) · [`resources`](#resources) · [`keep_awake`](#keep_awake) · [`sysmodule`](#sysmodule) · [`hekate_entries`](#hekate_entries) · [`reboot`](#reboot) |
| [CLI Command Reference](#7-cli-command-reference) | Every tool, plus setup, from a terminal: `kickstand <command>` |

---

## 1. Develop & Run Homebrew

Tools for building, deploying, launching, and supervising homebrew NROs and installed titles on the console.

### `test_build`
Build, deploy, run, and watch a project in one call (configured in `~/.config/kickstand/projects.json`; list them with [`projects`](#projects)).
- Runs the project's build command on the PC (returning the tail of the build log on failure).
- Verifies that the `.nro` was produced from the `.elf`.
- Uploads the `.nro` to its configured destination on the SD card and launches it with **full memory**.
- Supervises execution just like [`supervise`](#supervise), returning the build result, outcome (`EXITED`, `CRASHED` with a symbolized report, `FATAL`, `LOOKS HUNG` with thread backtraces, or `STILL RUNNING`), recent stdout, and a screenshot.
- Remembers the project's ELF for subsequent memory and thread tools, and keeps stdout readable via [`nro_output`](#nro_output). Closes whatever application is currently running first.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `project` | `string` *(required)* | — | Project name from `~/.config/kickstand/projects.json`. |
| `skip_build` | `boolean` | `false` | Deploy and run the existing binary as-is without running the build command. |
| `args` | `string[]` | Project's default | Command-line arguments (`argv[1..]`) passed to the NRO. |
| `seconds` | `integer` | `60` *(max 300)* | How many seconds to watch the running NRO before returning `STILL RUNNING`. |
| `quiet_seconds` | `integer` | `20` | Seconds without stdout before checking whether the screen is frozen for a hang. |

---

### `projects`
List the projects known to [`test_build`](#test_build) from `~/.config/kickstand/projects.json`, showing each project's name, directory, build command, local NRO path, SD card destination, and launch arguments (`args`, which `test_build`'s own `args` replace for one run).

*Parameters: none.*

---

### `run_nro`
Start a homebrew NRO (either uploaded from `local_path` on the PC or already on the SD card at `sd_path`) with arguments via `hbmenu`'s NetLoader, and stream its initial stdout.
- **`mode: "full"` (default):** Runs the NRO with the console's full memory in place of a host title (see [`open_hbmenu`](#open_hbmenu)), closing any currently running application first. Required for games.
- **`mode: "applet"`:** Runs small tools quickly from the Album applet (a few hundred MB of RAM). Refused for projects listed in `projects.json`.
- With `sd_path`, hbmenu is sent the Kickstand NRO as a launcher, which then opens the NRO on the card (NetLoader can only run what it's sent); the result says so.
- If an `elf` is provided, `<module>+0x<offset>` addresses printed to stdout are symbolized live with function names and source lines, and the ELF is remembered for [`threads`](#threads), [`memory_map`](#memory_map), [`read_memory`](#read_memory), and [`write_memory`](#write_memory).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `local_path` | `string` | — | Path to an `.nro` on the PC to upload and run (provide `local_path` or `sd_path`). |
| `dest` | `string` | `/switch/kickstand/<filename>` | Where to place `local_path` on the SD card (must be under `/switch/` in an existing folder). |
| `sd_path` | `string` | — | Instead of `local_path`: an `.nro` already on the SD card (e.g. `/switch/mygame/mygame.nro`). |
| `args` | `string[]` | `[]` | Arguments for the NRO (`argv[1..]`). |
| `wait_seconds` | `integer` | `5` *(max 60)* | Seconds to collect stdout before returning (returns earlier if the NRO exits). |
| `elf` | `string` | — | Path to the NRO's `.elf` on the PC (with debug info) for live backtrace symbolization. |
| `module` | `string` | ELF filename stem | Module name used in backtraces. |
| `mode` | `string` | `"full"` | `"full"` (title takeover with full RAM) or `"applet"` (Album applet mode for small tools). |

---

### `nro_output`
Read additional stdout emitted by the NRO that [`run_nro`](#run_nro) or [`test_build`](#test_build) last started, and check whether its stdout stream is still open (running) or closed (exited).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `wait_seconds` | `integer` | `0` *(max 60)* | Let the NRO run for up to this many seconds before reading new output (returns early on exit). |
| `grep` | `string` | — | Only lines matching this regular expression (e.g. `"report=\|error"`). Lines left out are still marked read. |
| `after` | `integer` | `0` | With `grep`: also show this many lines after each match (e.g. a report's top entries). |
| `max_lines` | `integer` | — | Show at most this many lines, the newest (after `grep`). |
| `timestamps` | `boolean` | `false` | Start each line with when it arrived, in seconds since the NRO started (`[12.345s] `), to time phases like boot or a state load. |

---

### `supervise`
Watch the currently running homebrew (started by [`run_nro`](#run_nro)) until an event occurs, and report the outcome:
- **`EXITED`:** The NRO exited cleanly; returns its final stdout.
- **`CRASHED`:** Atmosphère recorded a new crash report for the process; returns the symbolized report and Atmosphère's crash screenshot.
- **`FATAL`:** The console hit an Atmosphère fatal error screen; automatically warm-reboots into Hekate entry 1 (unless `reboot_on_fatal` is `false`).
- **`LOOKS HUNG`:** No stdout for `quiet_seconds` and an unchanged screen for 5 seconds more; pauses the process and returns every thread's symbolized backtrace plus a screenshot.
- **`STILL RUNNING`:** `seconds` elapsed with the NRO still active.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `seconds` | `integer` | `60` *(max 300)* | Maximum seconds to watch before returning `STILL RUNNING`. |
| `quiet_seconds` | `integer` | `20` | Seconds without stdout before checking the screen for a hang. |
| `elf` | `string` | Last ELF given | Path to the NRO's `.elf` on the PC for symbolizing crash reports and thread stacks. |
| `reboot_on_fatal` | `boolean` | `true` | Automatically warm-reboot into Hekate entry 1 if the console hits a fatal screen. |

---

### `open_hbmenu`
Open the Homebrew Menu (`hbmenu`).
- **`mode: "applet"` (default):** Opens `hbmenu` via the home menu's Album button. Fast and leaves any running game suspended, but only has a few hundred MB of RAM.
- **`mode: "full"`:** Launches `hbloader` in place of a game title with full console memory. The daemon arms Atmosphère's HBL override config for that single launch so no controller button needs to be held. Tries the previously remembered host title (`~/.config/kickstand/host-title`) or walks the home menu row until a title succeeds.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `mode` | `string` | `"applet"` | `"applet"` or `"full"`. |

---

### `close_app`
Close the running homebrew or game.
- **`"applet"`:** Closes the Album applet cleanly by pressing `HOME` (never terminates the applet process, which would crash `qlaunch`).
- **`"application"`:** Terminates the running full-memory homebrew or game immediately (without saving).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `target` | `string` | `"applet"` if open, else `"application"` | `"applet"` or `"application"`. |

---

### `list_apps`
List installed titles on the console with their 16-digit hex Title IDs and names, ordered most-recently-played first (matching the Switch home menu row).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `max` | `integer` | `12` *(max 64)* | Maximum number of titles to list (`12` is the main home menu row before *All Software*). |

---

### `launch_title`
Launch an installed game or application from the home menu row as a player would: locates its tile among the 12 most recent titles, presses `A`, and selects a user profile if prompted. Closes any currently running application first.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `title` | `string` *(required)* | — | 16-digit hex Title ID or substring of the title's name (from [`list_apps`](#list_apps)). |
| `user` | `integer` | `0` | 0-based index of the user profile to select on the user picker screen. |

---

## 2. Screen Vision & Capture

Tools for inspecting the console's screen, waiting on visual transitions, and recording motion.

### `screenshot`
Capture the Switch's screen (`1280x720` JPEG). Each screenshot is assigned a sequential **shot number** (the last 12 full-resolution shots are cached in memory for [`input_until`](#input_until)). The on-screen Kickstand keep-awake indicator is automatically hidden from captures.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `width` | `integer` | `1280` | Scale the returned image to this width in pixels (e.g. `640` uses ~1/4 the tokens). |
| `region` | `string` | Full screen | Crop to `"x,y,w,h"` in `1280x720` screen coordinates to read small text or UI at full resolution. |
| `grid` | `boolean` | `false` | Overlay a labelled coordinate grid every 100 pixels for picking `region` or touch coordinates. |
| `local_path` | `string` | — | Also save the full-resolution `1280x720` JPEG to this path on the PC. |

---

### `wait_screen`
Wait for the Switch's screen (or a specific `region`) to either **change** (e.g. a loading screen finishes or a dialog appears) or become **still** (e.g. a fade or scrolling animation settles), then return the resulting screenshot in one call instead of polling.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `until` | `string` | `"change"` | `"change"` (wait until pixels differ) or `"still"` (wait until pixels stop changing). |
| `timeout_seconds` | `number` | `10.0` *(max 60)* | Maximum seconds to wait before returning the current screen. |
| `region` | `string` | Full screen | Only watch `"x,y,w,h"` in `1280x720` coordinates, ignoring animation outside it. |
| `width` | `integer` | `640` | Width in pixels of the returned screenshot (`1280` for full resolution). |

---

### `record_gif`
Record the Switch's screen as a looping GIF from repeated captures and save it on the PC (defaulting to `~/Pictures/kickstand/gif-<timestamp>.gif`). Also returns a JPEG contact sheet of up to 6 evenly spaced frames so the agent can inspect the motion directly.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `local_path` | `string` | `~/Pictures/kickstand/gif-<ts>.gif` | Path on the PC to save the GIF. |
| `seconds` | `number` | `3.0` *(max 30)* | Duration to record in seconds. |
| `fps` | `integer` | `5` *(max 10)* | Target frames per second. |
| `width` | `integer` | `640` | Width of the GIF in pixels (`64`–`1280`). |

---

## 3. Controller, Keyboard & Macros

Tools for driving menus and gameplay via a virtual Pro Controller, touch screen and USB keyboard, and for replaying input the user recorded.

### `press`
Press one or more buttons simultaneously on a virtual Pro Controller (attaches the controller, waits ~1s to settle, performs the presses, and detaches).
- **Button names:** `a`, `b`, `x`, `y`, `l`, `r`, `zl`, `zr`, `plus`, `minus`, `up`, `down`, `left`, `right`, `lstick`, `rstick`, `home`, `capture` (join chords with `+`, e.g. `"zl+zr"`).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `buttons` | `string` *(required)* | — | Button or `+`-joined button combo to press (e.g. `"a"`, `"home"`, `"zl+zr"`). |
| `times` | `integer` | `1` *(1–10)* | Number of times to press the button(s). |
| `hold_ms` | `integer` | `100` *(1–2000)* | Milliseconds to hold each press. |
| `then` | `string` | — | Then look, in the same call: `"still"` waits for the screen to settle (menus, fades) and returns it; `"change"` waits for it to change. Up to 10 s, like [`wait_screen`](#wait_screen). |
| `width` | `integer` | `640` | Width of that screenshot. |

---

### `input`
Play a timed sequence of inputs on a single virtual controller for analog sticks, touch screen taps/drags, button holds across steps, and quick combos. Everything is released at the end.
- **Step syntax:** Space-separated `WHAT:MS` tokens (up to 64 steps, 20s total).
  - Buttons: `a`, `b`, `x`, `y`, `l`, `r`, `zl`, `zr`, `plus`, `minus`, `up`, `down`, `left`, `right`, `lstick`, `rstick`, `home`, `capture`
  - Analog sticks: `l=X,Y` / `r=X,Y` (`-1.0..1.0`, `Y` is up; centered if omitted)
  - Touch screen: `touch=X,Y` in `1280x720` coordinates
  - Pause (nothing held): `-:MS`
  - Combine simultaneous inputs with `+`: e.g. `"l=1,0+b:800"` (run right for 800 ms), `"r:300 r+a:150 r:6000"`, `"touch=640,360:100"`.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `steps` | `string` *(required)* | — | Space-separated `WHAT:MS` input steps. |
| `then` | `string` | — | `"still"` or `"change"`: return the screen afterwards, as in [`press`](#press). |
| `width` | `integer` | `640` | Width of that screenshot. |

---

### `input_until`
Closed-loop input that holds a direction/button, mashes a button repeatedly, or loops an input sequence while watching the screen several times a second, stopping the moment a visual condition is met (~250 ms reaction time).
- **Actions (provide exactly one):**
  - `hold`: Hold buttons/sticks continuously (e.g. `"right"`, `"l=1,0+b"`).
  - `repeat`: Press button(s) repeatedly every `every_ms` (e.g. `"a"` to advance dialogue).
  - `steps`: Replay an [`input`](#input) step sequence in a loop.
- **Stop conditions (`until`):**
  - `"change"`: Stop when the screen (or `region`) changes from when the call began.
  - `"still"`: Stop when the screen (or `region`) stops changing.
  - `"appears"`: Stop when the patch cropped from `region` of `shot` appears within `search`.
  - `"gone"`: Stop when the patch cropped from `region` of `shot` disappears from `search`.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `until` | `string` *(required)* | — | `"change"`, `"still"`, `"appears"`, or `"gone"`. |
| `hold` | `string` | — | Input to hold continuously (without `:MS`, e.g. `"l=1,0+b"`). |
| `repeat` | `string` | — | Button(s) to press repeatedly (e.g. `"a"`). |
| `every_ms` | `integer` | `400` *(min 150)* | Milliseconds between `repeat` presses. |
| `steps` | `string` | — | [`input`](#input) steps to replay in a loop. |
| `region` | `string` | — | `"x,y,w,h"` in `1280x720` coordinates (required for `"appears"` and `"gone"`). |
| `shot` | `integer` | Newest shot | Screenshot number (from [`screenshot`](#screenshot) or [`wait_screen`](#wait_screen)) to crop `region` from. |
| `search` | `string` | 96 px around `region` | Where to search for the patch: `"x,y,w,h"` or `"screen"`. |
| `timeout_seconds` | `number` | `15.0` *(max 60)* | Give up after this many seconds. |
| `width` | `integer` | `640` | Width in pixels of the returned screenshot. |

---

### `type_text`
Type text on a virtual USB keyboard: into the software keyboard (naming a save, a profile, a search) or anything else that reads a keyboard. About 14 characters a second, US layout. Open the keyboard first (with [`press`](#press)), then check it took the text with [`screenshot`](#screenshot).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `text` | `string` *(required)* | — | Printable ASCII, tabs and newlines, up to 250 characters a call. |
| `enter` | `boolean` | `false` | Press Enter afterwards (e.g. to confirm the software keyboard). |

---

### `macros`
List the macros saved on this PC: controller input the user recorded once (in the desktop app's **Macros** page, with `kickstand macro record`, or through [`record_macro`](#record_macro)) for agents to replay with [`play_macro`](#play_macro), e.g. to get from the title screen to a level. Each comes with its length, when it was recorded and its note (what it does and where it starts). No parameters.

Macros live in `~/.config/kickstand/macros/<name>.json` as `input` step text, so they can be read and edited by hand.

---

### `record_macro`
Record a macro from the physical controllers while the user plays. Call `start`, tell the user what to play (and to put the controller down when done), then `stop` with a name and a note. Records the buttons and both sticks of every connected controller, merged; HOME, Capture and touch aren't recorded. Idle time at the start and end is trimmed. Only useful with someone at the console.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `action` | `string` *(required)* | — | `"start"` or `"stop"`. |
| `name` | `string` | — | When stopping: the macro's name (letters, digits, spaces, `-` and `_`), e.g. `"title to rogueport"`. |
| `note` | `string` | — | When stopping: what it does and where it starts from, for whoever plays it later. |
| `max_seconds` | `integer` | `300` *(max 600)* | When starting: stop by itself after this many seconds. |

---

### `play_macro`
Replay a saved macro on a virtual controller with its recorded timing, e.g. to get back to the same place in a game before a test. Start from where it was recorded (its note says where). Timing can drift a little over a long macro, so check where it ended (`then: "still"`) and fix up with [`press`](#press) or [`input_until`](#input_until). Long macros are streamed to the console in pieces, without gaps.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `name` | `string` *(required)* | — | A saved macro's name (see [`macros`](#macros)). |
| `then` | `string` | — | `"still"` or `"change"`: return the screen afterwards, as in [`press`](#press). |
| `width` | `integer` | `640` | Width of that screenshot. |

---

## 4. Debugging, Crash Analysis & Memory

Tools for diagnosing crashes, inspecting hung threads, symbolizing addresses, and reading or modifying live memory.

### `crash_reports`
List the newest Atmosphère process crash reports and fatal error records on the SD card, newest first, with a one-line summary for each.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `limit` | `integer` | `5` | Maximum number of crash reports/fatal records to list. |

---

### `crash_report`
Fetch and summarize a specific crash report (process, exception type, fault result, crashed thread registers, stack backtrace, and loaded modules) or decode a fatal record, including Atmosphère's crash screenshot if one was saved. When `elf` is provided, `<module> + 0x...` offsets are resolved to function names and DWARF source lines.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `name` | `string` *(required)* | — | Path or filename from [`crash_reports`](#crash_reports) (e.g. `02637676358_01006fe013472000.log` or `report_52b130e42.bin`). |
| `elf` | `string` | — | Path to the crashed module's `.elf` on the PC for symbolization. |
| `module` | `string` | ELF filename stem | Module name used in the report. |

---

### `symbolize`
Resolve `<module>+0x<offset>` addresses (or bare hex offsets, one per line) in arbitrary text or log output into function names and source lines (`file:line`) using `addr2line` against an ELF binary on the PC.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `elf` | `string` *(required)* | — | Path to the module's `.elf` on the PC (with debug info). |
| `text` | `string` *(required)* | — | Text containing `<module>+0x<offset>` addresses or bare hex offsets. |
| `module` | `string` | ELF filename stem | Module name matched in `text`. |

---

### `threads`
Pause the running homebrew briefly, read every thread's registers, walk each thread's frame pointers, and symbolize the backtraces (with function names and DWARF source lines) using the NRO's ELF. Ideal for diagnosing deadlocks, GPU fence waits, or infinite loops when a build hangs. Warns if the ELF's build ID/layout doesn't match the running NRO.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `elf` | `string` | Last ELF given | Path to the NRO's `.elf` on the PC. |

---

### `memory_map`
Return the running application's virtual memory map (each region's start/end address, size, permissions, and type) and locate where the homebrew NRO is loaded in memory.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `elf` | `string` | Last ELF given | Path to the NRO's `.elf` on the PC, used to label which symbol each NRO region starts at. |

---

### `read_memory`
Read memory from the running application (homebrew or game) while it runs. Briefly attaches as a debugger so each read is a consistent snapshot.
- **Address expression syntax (`address`):**
  - Absolute address: `0x7100001000`
  - Relative to NRO base: `nro+0x1234` (or `<ModuleName>+0x1234`)
  - Relative to heap base: `heap+0x800`
  - ELF symbol name: `gPlayerStatus`, `gCurrentEncounter+0x10`
  - Pointer dereference chains (`[expr]` reads a 64-bit pointer at `expr`): `[[gGameState]+0x20]+0x8`

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `address` | `string` *(required)* | — | Address expression (`0x…`, `nro+0x…`, `heap+0x…`, symbol, `+`/`-` offsets, `[expr]` dereferences). |
| `format` | `string` | `"hex"` | `"hex"` (hexdump), `"u8"`, `"u16"`, `"u32"`, `"u64"`, `"i8"`, `"i16"`, `"i32"`, `"i64"`, `"f32"`, `"f64"`, `"ptr"` (named when pointing into the NRO), or `"str"` (NUL-terminated string). |
| `length` | `integer` | 1 value (`64` for `hex`, `256` for `str`; max 1 MiB) | Number of bytes to read. |
| `elf` | `string` | Last ELF given | Path to the NRO's `.elf` on the PC for symbol lookup. |
| `module` | `string` | ELF filename stem | Module name for `<module>+0x…` expressions. |

---

### `write_memory`
Write a value to the running **homebrew's** memory (strictly refused for retail games) and return the value before and after the write. Uses the same address expression syntax as [`read_memory`](#read_memory).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `address` | `string` *(required)* | — | Address expression (same syntax as [`read_memory`](#read_memory)). |
| `value` | `string` *(required)* | — | Value to write: number for int/float/ptr formats (`0x…` allowed), text for `"str"` (writes trailing NUL), or hex byte string for `"hex"`. |
| `format` | `string` *(required)* | — | `"u8"`, `"u16"`, `"u32"`, `"u64"`, `"i8"`, `"i16"`, `"i32"`, `"i64"`, `"f32"`, `"f64"`, `"ptr"`, `"str"`, or `"hex"`. |
| `elf` | `string` | Last ELF given | Path to the NRO's `.elf` on the PC. |
| `module` | `string` | ELF filename stem | Module name for `<module>+0x…` expressions. |

---

## 5. SD Card Files & Logs

Tools for inspecting and transferring files on the Switch's SD card.

> **Access control:** Kickstand's own folders, `/atmosphere/crash_reports`, `/atmosphere/fatal_reports`, and project destination folders from `projects.json` are accessible automatically. For any other SD card folder, Kickstand prompts the user in the AI app (*"Just this once"*, *"This folder, until the app restarts"*, or *"This folder, from now on"*), or in its own desktop window when the AI app can't show the prompt (the agent asks in the chat at the same time, and passes the answer on with [`answer_access`](#answer_access), which takes the question out of the window), or the user can pre-authorize folders in a terminal with `kickstand allow <folder> [--write]`. System folders (`/atmosphere` outside crash reports, `/bootloader`, `/Nintendo`, keys, and NAND backups) are hardware-blocked by the daemon.

### `list_dir`
List entries in a directory on the SD card (sorted directories-first, with file sizes).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` *(required)* | — | Absolute path from the SD card root (e.g. `/switch` or `/atmosphere/crash_reports`). |

---

### `stat`
Return whether an SD card path is a file or directory and its size in bytes.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` *(required)* | — | Absolute path from the SD card root. |

---

### `read_file`
Read a slice of a file on the SD card. Returns UTF-8 text directly or base64-encoded data for binary files.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` *(required)* | — | Absolute path from the SD card root. |
| `offset` | `integer` | `0` | Byte offset to start reading from. |
| `length` | `integer` | `65536` *(max 1048576)* | Maximum number of bytes to read. |

---

### `tail_file`
Read the last lines of a log file on the SD card and optionally follow new lines as the file grows. Returns a `next` byte offset that can be passed as `from` on a subsequent call to resume without gaps.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` *(required)* | — | Absolute path to the log file on the SD card. |
| `lines` | `integer` | `40` | Number of trailing lines to return when `from` is not given. |
| `from` | `integer` | — | Byte offset (`next` from an earlier `tail_file` call) to continue reading from. |
| `follow_seconds` | `number` | `0` *(max 60)* | Poll every 0.5s for up to this many seconds to collect newly appended output. |
| `until` | `string` | — | Stop following early as soon as a newly added line contains this substring. |

---

### `find`
Find files and folders by name under a folder on the SD card. The search runs on the console, so no folder listings cross Wi-Fi. Skips what Kickstand never reads, and stops after 20 s with what it has (saying so).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `dir` | `string` *(required)* | — | The folder to search under, e.g. `/switch`. |
| `name` | `string` | `""` | Part of the name to look for, ignoring case (e.g. `".nro"`, `"save"`); empty lists everything. |
| `max` | `integer` | `100` *(max 1000)* | Most results. |

---

### `grep`
Search a file, or every file under a folder, for lines containing some text. The search runs on the console and returns the matching lines with file and line number, without downloading anything. Binary files and files over 16 MB are skipped; long lines are cut to 300 characters.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` *(required)* | — | A file, or a folder to search every file under. |
| `text` | `string` *(required)* | — | The text to look for (plain text, not a pattern). |
| `ignore_case` | `boolean` | `false` | Ignore ASCII case. |
| `max` | `integer` | `50` *(max 500)* | Most matching lines. |

---

### `upload`
Upload a file from the PC to the Switch's SD card. Verified with CRC-32 on the console before replacing the target file.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `local_path` | `string` *(required)* | — | Path to the source file on the PC. |
| `remote_path` | `string` *(required)* | — | Destination path on the SD card (e.g. `/switch/mygame/mygame.nro`). |

---

### `download`
Copy a complete file from the Switch's SD card to the PC.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` *(required)* | — | Source file path on the SD card. |
| `local_path` | `string` *(required)* | — | Destination path on the PC. |

---

### `delete`
Delete a file or directory on the Switch's SD card (within permitted writable folders).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` *(required)* | — | Absolute path on the SD card to delete. |
| `recursive` | `boolean` | `false` | Delete a non-empty directory and all of its contents. |

---

### `answer_access`
Pass on the user's answer to an access question they answered in the chat, for AI apps that can't show Kickstand's own prompt. Only works for a question Kickstand actually asked (a tool said so); it counts the same as answering in Kickstand's window, and takes the question out of that window. Then retry the request.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` *(required)* | — | The path the request was about, as the tool said. |
| `answer` | `string` *(required)* | — | `once` (just this once), `session` (this folder until Kickstand restarts), `always` (this folder from now on) or `no`. |

---

## 6. Console & Daemon Management

Tools for checking console state, managing the background sysmodule, keeping the console awake, and warm-rebooting into Hekate.

### `status`
One-call snapshot of the console's state:
- Running application (game, or `hbloader` with `hbmenu`/NRO in full-memory mode) and running applet (`hbloader` in the Album).
- Whether the console is stuck on an Atmosphère fatal error screen.
- Whether the sysmodule is still pressing `B` to dismiss the lock screen after a reboot.
- Remaining keep-awake lease time, console uptime, any other AI agents currently sharing the console, and a small screenshot (if the console refuses one, the rest is still returned with a note).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `screenshot` | `boolean` | `true` | Include a downscaled screenshot of the current screen. |
| `width` | `integer` | `480` | Width of the status screenshot in pixels. |

---

### `ping`
Verify that the Kickstand daemon on the Switch is reachable, reporting round-trip latency, console uptime, daemon version, protocol version, and whether it is running as the background `Sysmodule` or the `Nro`.

*Parameters: none.*

---

### `resources`
Report kernel resource-limit usage of the daemon's process group (memory, threads, events, transfer memories, and IPC sessions: current, peak, and limit) plus the daemon's own heap usage. When running as the sysmodule, this reflects the system-wide resource pool shared by all background sysmodules.

*Parameters: none.*

---

### `keep_awake`
Manage the console's keep-awake lease (indicated by a small Kickstand icon next to the Switch's clock). Every tool call automatically renews a 30-minute lease; after it expires, the console's normal auto-sleep settings resume.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `minutes` | `integer` | `30` *(max 480)* | Minutes from now to keep the console awake (`0` ends the lease immediately so it can sleep). |

---

### `sysmodule`
Manage the `sys-kickstand` background sysmodule on the Switch.
- **`"status"`:** Check whether the sysmodule is installed, whether `boot2.flag` is set, and its running PID.
- **`"install"`:** Upload a built `sys-kickstand.nsp` and live-restart onto the new build in ~11–20s without rebooting the console (by briefly launching the Kickstand NRO in applet mode to hand over).
- **`"start"` / `"stop"` / `"handover"`:** Start, stop, or hand over from the NRO daemon to the sysmodule.
- **`"enable"` / `"disable"`:** Create or remove `boot2.flag` (start at boot; requires explicit user consent).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `action` | `string` *(required)* | — | `"status"`, `"install"`, `"start"`, `"stop"`, `"handover"`, `"enable"`, or `"disable"`. |
| `local_path` | `string` | — | For `"install"`: path to `sys-kickstand.nsp` on the PC. |
| `nro_path` | `string` | — | For `"install"`: optional newer `kickstand.nro` to upload before restarting. |
| `restart` | `boolean` | `true` | For `"install"`: immediately restart onto the new sysmodule build (`false` only uploads it). |

---

### `hekate_entries`
List the boot entries configured in `/bootloader/hekate_ipl.ini`, numbered `1..N` as expected by [`reboot`](#reboot).

*Parameters: none.*

---

### `reboot`
Warm-reboot the Switch into Hekate via Atmosphère's `reboot-to-payload` (never performs a cold boot, preserving RCM-injected setups). Requires explicit user confirmation (`confirm: true`).

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `target` | `string` *(required)* | — | `"menu"` (Hekate main menu), `"ums"` (SD card USB Mass Storage), or a boot entry name/1-based number from [`hekate_entries`](#hekate_entries). |
| `confirm` | `boolean` *(required)* | — | Must be `true`, and only set after the user has agreed to reboot. |

---

## 7. CLI Command Reference

Every capability above is also available directly from your terminal via `kickstand <command>` (run `kickstand <command> --help` for full flag details):

| Category | Command | Description |
| :--- | :--- | :--- |
| **App & Setup** | `kickstand` / `kickstand desktop` | Open the desktop GUI to find the Switch, pair, link AI apps, and watch live agent activity. |
| | `kickstand serve [--app <name>]` | Run the MCP server over `stdio` (invoked automatically by linked AI apps). |
| | `kickstand install [--remove]` | Add (or remove) Kickstand in your OS application menu with its icon. |
| | `kickstand discover [--to <ip>]` | Find Switches running Kickstand on the local network. |
| | `kickstand pair <code>` | Pair with the Switch displaying `CODE` and save its address and shared token. |
| | `kickstand update [--check] [--switch]` | Update this PC's copy to the latest signed release, then the Switch to the PC's version (only while it's idle). |
| | `kickstand unpair` | Make the Switch forget this PC's token and stop using it; pair again with the code to come back. |
| | `kickstand link [app] [--remove]` | Link Kickstand to `code`, `desktop`, `gemini`, `agy`, `antigravity`, `codex`, or `lmstudio`. |
| | `kickstand token` | Print (or generate) the shared authentication token and its SD card location. |
| | `kickstand allow [folder] [--write] [--remove]` | Pre-authorize an SD card folder for agent access (must be run interactively in a terminal). |
| **Develop & Run** | `kickstand test <project>` | Build, deploy, run, and supervise a project from `~/.config/kickstand/projects.json`. |
| | `kickstand projects` | List projects configured in `~/.config/kickstand/projects.json`. |
| | `kickstand run <nro> [-- <args>...]` | Run a local `.nro` or `sdmc:/path.nro` with full memory (or `--applet`) and stream stdout. |
| | `kickstand supervise` | Watch the running homebrew until it exits, crashes, fatals, or hangs. |
| | `kickstand hbmenu [--full]` | Open `hbmenu` in applet mode or `--full` title-takeover mode. |
| | `kickstand apps [--max <n>]` | List installed titles in home-menu order. |
| | `kickstand launch <title> [--user <n>]` | Launch an installed title by ID or name substring. |
| | `kickstand close-app [--applet]` | Close the running application or (`--applet`) the Album applet. |
| | `kickstand app` | Show the currently running application and whether it is `hbloader`. |
| **See & Play** | `kickstand watch [--bind 0.0.0.0]` | Serve a live web page (`http://127.0.0.1:8790`) streaming the console's screen and status. |
| | `kickstand screenshot <file.jpg>` | Save a `1280x720` JPEG screenshot of the console. |
| | `kickstand wait [change\|still]` | Wait until the screen (or `--region`) changes or stops changing. |
| | `kickstand gif <file.gif>` | Record the screen as a looping GIF. |
| | `kickstand press <buttons>` | Press buttons on a virtual controller (or `unlock` to press `B` three times). |
| | `kickstand input <steps>` | Play timed `WHAT:MS` steps (buttons, sticks, touch). |
| | `kickstand type <text> [--enter]` | Type text on a virtual USB keyboard (into the software keyboard, for one). |
| | `kickstand macro record <name> [--note <text>] [--seconds <n>]` | Record the controllers while you play, until Enter is pressed in the terminal, and save it as a macro. |
| | `kickstand macro play <name>` | Replay a saved macro. |
| | `kickstand macro list` / `delete <name>` | List or delete saved macros. |
| **Debug & Memory** | `kickstand crashes [--limit <n>]` | List the newest Atmosphère crash reports and fatal records. |
| | `kickstand crash <name> [--elf <elf>]` | Summarize a crash report or fatal record (with optional symbolization and `--screenshot`). |
| | `kickstand symbolize <elf> [text]` | Symbolize `<module>+0x<offset>` addresses in text or `stdin`. |
| | `kickstand threads [--elf <elf>]` | Pause the running homebrew and print symbolized backtraces for all threads. |
| | `kickstand memory [--elf <elf>]` | Print the running application's memory map and NRO load address. |
| | `kickstand peek <addr> [--as <fmt>]` | Read memory from the running application by address, symbol, or pointer chain. |
| | `kickstand poke <addr> <val> --as <fmt>` | Write to the running homebrew's memory. |
| **SD Card Files** | `kickstand ls <path>` | List a directory on the SD card. |
| | `kickstand cat <path>` | Print a file from the SD card. |
| | `kickstand get <remote> <local>` | Download a file from the SD card to the PC. |
| | `kickstand put <local> <remote>` | Upload a file from the PC to the SD card (CRC-32 verified). |
| | `kickstand tail [-f] [-n <lines>] <path>` | Print the last lines of an SD card file (and follow with `-f`). |
| | `kickstand find <dir> [name] [--max <n>]` | Find files and folders by name, searched on the console. |
| | `kickstand grep [-i] <text> <path> [--max <n>]` | Print matching lines of a file or of every file under a folder, searched on the console. |
| | `kickstand rm [-r] <path>` | Delete a file or directory on the SD card. |
| **Console** | `kickstand status` | Show uptime, unlock state, keep-awake timer, running app, and applet status. |
| | `kickstand ping` | Ping the daemon and report round-trip time and version. |
| | `kickstand resources` | Show kernel resource limit usage of the daemon's group. |
| | `kickstand keep-awake [minutes]` | Set or check the keep-awake timer (`0` to end immediately). |
| | `kickstand sysmodule <subcommand>` | `status`, `install <nsp>`, `start`, `stop`, `handover`, `enable`, or `disable`. |
| | `kickstand reboot [target] [--yes]` | Warm-reboot into Hekate (`menu`, `ums`, or an entry name/number). |
| | `kickstand exit` | Stop the NRO daemon and return to `hbmenu`. |
