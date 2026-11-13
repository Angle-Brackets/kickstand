# Kickstand Capabilities & MCP Tools Reference

This document details all **47 MCP tools** exposed by `kickstand serve` and the matching **CLI commands** provided by the `kickstand` binary.

## At a Glance

The tool names are the same in every AI app.

| Area | Tools |
| :--- | :--- |
| [Develop & Run Homebrew](#1-develop--run-homebrew) | [`test_build`](#test_build) · [`projects`](#projects) · [`run_nro`](#run_nro) · [`reload_nro`](#reload_nro) · [`nro_output`](#nro_output) · [`supervise`](#supervise) · [`open_hbmenu`](#open_hbmenu) · [`quit_app`](#quit_app) · [`force_quit_app`](#force_quit_app) · [`list_apps`](#list_apps) · [`launch_title`](#launch_title) |
| [Screen Vision & Capture](#2-screen-vision--capture) | [`screenshot`](#screenshot) · [`wait_screen`](#wait_screen) · [`record_gif`](#record_gif) · [`audio_replay`](#audio_replay) · [`record_audio`](#record_audio) · [`audio_control`](#audio_control) |
| [Controller, Keyboard & Macros](#3-controller-keyboard--macros) | [`press`](#press) · [`input`](#input) · [`input_until`](#input_until) · [`type_text`](#type_text) · [`macros`](#macros) · [`record_macro`](#record_macro) · [`save_macro`](#save_macro) · [`play_macro`](#play_macro) · [`control_macro`](#control_macro) |
| [Debugging, Crash Analysis & Memory](#4-debugging-crash-analysis--memory) | [`crash_reports`](#crash_reports) · [`crash_report`](#crash_report) · [`symbolize`](#symbolize) · [`threads`](#threads) · [`memory_map`](#memory_map) · [`read_memory`](#read_memory) · [`write_memory`](#write_memory) |
| [SD Card Files & Logs](#5-sd-card-files--logs) | [`list_dir`](#list_dir) · [`stat`](#stat) · [`read_file`](#read_file) · [`tail_file`](#tail_file) · [`find`](#find) · [`grep`](#grep) · [`upload`](#upload) · [`usb_upload`](#usb_upload) · [`download`](#download) · [`delete`](#delete) · [`answer_access`](#answer_access) · [`answer_hint`](#answer_hint) |
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

### `reload_nro`
Swap a new build of the homebrew that's running for the old one, without going back to the Home Menu. For the edit, build, test loop: much faster than [`run_nro`](#run_nro), which reopens hbmenu from the Home Menu every time.
- Uploads the build to the SD card while the old one still runs (over USB when a cable is connected), then presses `+` so the old one exits. Homebrew that returns from `main` stays inside the same hbloader process, and Kickstand starts the new build there, by the fastest of:
  1. **`hook`**: the app implements Kickstand's [next-load hook](#the-next-load-hook), usually by linking [libkickstand](#libkickstand), and starts the new build itself. hello.nro on hardware: **1.9 s** for the whole call, of which 1.4 s is the press itself (Kickstand attaches its virtual controller and gives it a second to be noticed; the app exits and the new build starts while `+` is still held). A large app that tears down before claiming takes longer: see the hook's notes.
  2. **`hbmenu`**: the app exits to hbmenu, Y starts its NetLoader, and the Kickstand NRO is sent to open the build (hello.nro: 4.4 s).
  3. **`cold`**: the same as `run_nro` (13-23 s, longer when an NRO has to be closed first), when no homebrew was running or it didn't come back to hbmenu.
- The result's first line names the path taken and the time per phase: `upload` (to the SD card), `exit` (from pressing `+` until the old build was out of the way) and `load` (until the new build's stdout connected). On the `hook` path the exit and the load happen during the press, so they are one figure. Read the rest of the stdout with [`nro_output`](#nro_output).
- A running NRO keeps its own file open, so a build that can't replace it goes to `<name>.reload.nro` beside it, and later reloads alternate between the two.
- With `replay_macro`, plays a saved macro once the build is up, to get back to the state under test.
- **The libkickstand question.** When the new build doesn't link [libkickstand](#libkickstand), Kickstand asks the user, as a question in the AI app with options, whether the agent should add it to the app's build: *Yes, add it to the build*, *Not now*, or *Don't ask again for this app*. It knows a libnx (C/C++) build from an nx (Rust) one, so a yes comes back with the right steps for the agent. Asked once per app each time Kickstand starts; *Don't ask again* is kept in `~/.config/kickstand/hints.json`. In an AI app that can't show the question, the agent is asked to put it to the user with its own question tool and pass the answer on with [`answer_hint`](#answer_hint). [`supervise`](#supervise) asks too when a build crashes within two minutes of loading into the same process as one without libkickstand, and says that leftovers are the likely cause.
- How the old build is ended goes by the build that's running (recorded when Kickstand started it): the first build with libkickstand still exits one without it the old way, and the reloads after that are asked.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `project` | `string` | — | A project from `projects.json` (its NRO, destination, args and ELF), or give `local_path` / `sd_path`. |
| `build` | `boolean` | `false` | With `project`, run its build command first. |
| `local_path` | `string` | — | An `.nro` on the PC to upload and reload. |
| `dest` | `string` | Project's, or `/switch/kickstand/<filename>` | Where `local_path` goes on the SD card (under `/switch/`). |
| `sd_path` | `string` | — | Instead: an `.nro` already on the SD card. |
| `args` | `string[]` | Project's, or `[]` | Arguments for the NRO (`argv[1..]`). |
| `hook` | `boolean` | Detected | Use the app's next-load hook. By default it's tried when the NRO contains `kickstand/next-load` (or, for `sd_path`, when it worked for that NRO before); `false` never, `true` always. A build linked with [libkickstand](#libkickstand) is asked to quit without a button press. |
| `exit_buttons` | `string` | `"plus"` | The buttons that make the app exit, if not `+`. |
| `prefer_usb` | `boolean` | `true` | Upload over USB when a cable or SD card mount is there. |
| `replay_macro` | `string` | — | A saved macro to play once the new build is running. |
| `wait_seconds` | `integer` | `5` *(max 60)* | Seconds to collect stdout before returning. |
| `elf`, `module` | `string` | Project's | For symbolizing backtraces, as in `run_nro`. |
| `mode` | `string` | `"full"` | `"full"` or `"applet"`. |

#### libkickstand
A small static library for libnx (C or C++) homebrew, in [`libkickstand/`](libkickstand/), that makes `reload_nro` fast **and** safe ([Rust on nx](#libkickstand) below). Link it; there's nothing to call:
- **Quit on request.** `appletMainLoop()` returns `false` once Kickstand asks for a reload, just as when the Home Menu closes the app, so the app quits its usual way: no button press, and apps that only quit from a menu reload too. A main loop that doesn't call `appletMainLoop()` can poll `kickstand_quit_requested()` once a frame.
- **The next-load hook** below, done for you at `exit()`.
- **Cleanup.** The next build loads into the same process and heap, and libnx doesn't undo what an app leaves behind: a thread that was never closed keeps its stack mapped over heap pages, and JIT code memory keeps its pages too, so the next build's `malloc` runs into the holes and crashes (Uprising NX died 0.7 s into every second build; Paperboat after a while). libkickstand wraps the kernel calls that create these (`svcCreateThread`, `svcMapMemory`, `svcCreateCodeMemory`, `svcCreateTransferMemory` and their undoing) and, at exit, gives threads a second to finish, pauses for good those still running (Horizon can't end another thread), and undoes every mapping still outstanding. This runs on any exit, so quitting to hbmenu and starting another NRO is safe too. What it did is appended to `sdmc:/kickstand/libkickstand.log`.

Quitting on request and the hook only act when the app was started from a PC over nxlink; a launch from hbmenu by hand behaves as before. Kickstand recognises a build linked with it by the marker `kickstand-lib/1` in the NRO.

Build it with `make -C libkickstand` (devkitA64), then add to your Makefile:
```make
include /path/to/kickstand/libkickstand/kickstand.mk
LDFLAGS += $(KICKSTAND_LDFLAGS)
LIBS    := -L/path/to/kickstand/libkickstand -lkickstand $(LIBS)
```
or with CMake:
```cmake
target_link_options(mygame PRIVATE -Wl,-u,kickstand_link
    -Wl,--wrap=appletMainLoop -Wl,--wrap=svcCreateThread -Wl,--wrap=svcCloseHandle
    -Wl,--wrap=svcMapMemory -Wl,--wrap=svcUnmapMemory -Wl,--wrap=svcCreateCodeMemory
    -Wl,--wrap=svcControlCodeMemory -Wl,--wrap=svcCreateTransferMemory)
target_link_libraries(mygame PRIVATE /path/to/kickstand/libkickstand/libkickstand.a)
```

On hardware: Paperboat reloaded three times in a row with no button press (the request claimed 1.8-2.6 s after it was written), each build running normally afterwards, and hbmenu's memory after quitting was the same as a fresh hbmenu's. Uprising NX's second build ran (150 s+, rendering) where it had crashed in 0.7 s; at exit the library paused 5 threads still running and unmapped their 11 MiB of stacks and one JIT region.

**Rust (nx).** Rust homebrew on the [nx](https://github.com/aarch64-switch-rs/nx) crate doesn't go through libnx, so there's nothing for `--wrap` to catch; [`libkickstand/rust`](libkickstand/rust/) (crate `kickstand-nx`, `use kickstand`) does the same jobs with three calls. nx keeps thread stacks in the heap and maps nothing, so a finished thread leaves no hole; only threads still running matter.
```rust
let worker = kickstand::spawn(nx::thread::Builder::new(), || audio_loop())?; // tracked, instead of nx::thread::spawn
while !kickstand::quit_requested() && !plus_pressed() {                       // checked at most 5 times a second
    frame();
}
let done = kickstand::finish(); // just before main returns: claims the request; pauses tracked threads still running after 1 s
```
Depend on it by path or git (`kickstand-nx = { git = "https://github.com/Angle-Brackets/kickstand" }`), pointing its `nx` at the one your app uses. [hello.nro](daemon/hello/src/main.rs) uses it (`--stay --thread`). JIT code memory your app created is still yours to unmap before returning.

Limits: a paused thread stays in the process until hbloader ends (hbmenu's Home Menu exit, or `run_nro`), and counts against its thread limit: Uprising leaves 5 per run, so after many reloads in a row, start fresh with `run_nro`. Memory the app maps by other means (nvdrv buffers, shared memory) is freed by the services when their sessions close, which libnx does at exit.

#### The next-load hook
Without libkickstand, a few lines in your homebrew make `reload_nro` skip hbmenu and NetLoader altogether: just a file on the SD card and libnx's `envSetNextLoad`. (libkickstand does this for you, and the cleanup this can't.)

**The contract.** Before pressing `+`, Kickstand writes `sdmc:/kickstand/next-load`, two lines of text:
```
sdmc:/switch/mygame/mygame.nro
"sdmc:/switch/mygame/mygame.nro" "--level" "3" "5656a8c0_NXLINK_"
```
Line 1 is the NRO to load; line 2 is its argv string, which hbloader passes to the next NRO as it is (hbmenu's convention: every argument in double quotes, the NRO's own path first, and nxlink's address marker last, so the new build's stdout comes back to Kickstand). When your app has decided to quit (as soon as it knows, before tearing down: `envSetNextLoad` only fills in hbloader's buffer, so the teardown can follow; or just before returning from `main`), it should:
1. Do nothing unless it was started from a PC over nxlink (libnx: `__nxlink_host.s_addr != 0`; otherwise an argument ending in `_NXLINK_`). A launch from hbmenu by hand never follows the file.
2. Read the file, if it exists.
3. Delete it, and only if the delete worked, call `envSetNextLoad(line1, line2)`.
4. Finish quitting and return from `main` as usual.

The new build loads into the same process, so it inherits whatever the old one left behind: an app that returns without closing what it opened (Vulkan or nvdrv, JIT code memory) can crash the next build on its first allocations. This is true of any relaunch through hbmenu too, not just the hook. [libkickstand](#libkickstand) releases those leftovers.

The delete is what claims the request. If the app doesn't claim it within 15 s of the `+`, Kickstand deletes it itself and goes through hbmenu instead; only one of the two deletes can succeed, so Kickstand always knows which way it went, and an app can't follow a request Kickstand gave up on. Kickstand also clears a leftover request before `run_nro` and before any reload that doesn't use the hook. Kickstand notices the hook by the file name in your NRO, so nothing needs configuring.

**C (libnx)**, called once the app has decided to quit, or just before returning from `main()` (libnx mounts `sdmc:` by default):
```c
#include <switch.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>

// Kickstand's hot-reload hook: if Kickstand left a next-load request, claim it and let hbloader load that NRO next.
static void kickstand_next_load(void) {
    const char *request = "sdmc:/kickstand/next-load";
    if (!__nxlink_host.s_addr) return;          // not started from a PC: leave the file alone
    FILE *f = fopen(request, "r");
    if (!f) return;
    char path[512], argv[2048];
    bool ok = fgets(path, sizeof path, f) && fgets(argv, sizeof argv, f);
    fclose(f);
    if (ok && remove(request) == 0) {           // the delete claims it; if Kickstand got there first, do nothing
        path[strcspn(path, "\r\n")] = 0;
        argv[strcspn(argv, "\r\n")] = 0;
        envSetNextLoad(path, argv);
    }
}
```

**Rust ([nx](https://github.com/aarch64-switch-rs/nx), features `fs`)**, the same thing by hand (or use the `kickstand-nx` crate above):
```rust
fn kickstand_next_load() {
    use nx::fs::{self, FileOpenOption};
    const REQUEST: &str = "sdmc:/kickstand/next-load";
    // Not started from a PC over nxlink: leave the file alone.
    if !nx::hbl::get_argv().is_some_and(|a| a.contains("_NXLINK_")) {
        return;
    }
    if fs::initialize_fspsrv_session().is_err() || fs::mount_sd_card("sdmc").is_err() {
        return;
    }
    let Ok(mut file) = fs::open_file(REQUEST, FileOpenOption::Read()) else { return };
    let mut buf = [0u8; 2600];
    let mut len = 0;
    while let Ok(n @ 1..) = file.read_array(&mut buf[len..]) {
        len += n;
    }
    drop(file);
    let mut lines = core::str::from_utf8(&buf[..len]).unwrap_or("").lines();
    if let (Some(path), Some(argv)) = (lines.next(), lines.next())
        && fs::remove_file(REQUEST).is_ok() // the delete claims it
    {
        // nx copies both into hbloader's buffers; it just wants 'static strings.
        let _ = nx::hbl::set_next_load_entry(String::leak(path.into()), String::leak(argv.into()));
    }
}
```

Try it with hello.nro, which waits for `+` (or a request from Kickstand) with `--stay`: `kickstand run hello.nro -- --stay`, rebuild, then `kickstand reload hello.nro -- --stay`.

---

### `nro_output`
Read additional stdout emitted by the NRO that [`run_nro`](#run_nro) or [`test_build`](#test_build) last started, and check whether its stdout stream is still open (running), closed by the NRO (it exited), or dropped (the connection failed, e.g. the console slept or lost Wi-Fi: the NRO may still be running, and `supervise` keeps watching it by its screen).

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

### `quit_app`
Quit the running homebrew or game the way a person does, so it can save and flush its output first.
- **`"application"`** (a game, or hbmenu or an NRO with full memory): presses `HOME`, then **Close software** (`X`, then `A`) on the home menu. The system asks the program to exit: an SDL port gets `SDL_QUIT`, and libnx's `appletMainLoop()` returns `false`. No error message is shown. A hung program is ended this way too, without the error (the system ends a program that isn't holding off its exit). Waits up to 12 s for it to end; if it's still running (say the home menu couldn't be reached), says so and suggests [`force_quit_app`](#force_quit_app). Tested on hardware with hbmenu, PaperBoat and a hung NRO.
- **`"applet"`:** closes the Album with `HOME` (it's never terminated: that would crash the home menu).

When `run_nro`, `test_build`, `open_hbmenu` or `launch_title` replace what's running, they quit it the same way first, and only terminate it if it doesn't exit.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `target` | `string` | `"applet"` if open, else `"application"` | `"applet"` or `"application"`. |

---

### `force_quit_app`
Terminate the running application at once, whatever state it's in (like `kill -9`). It can't save or flush anything, and the home menu reports that the software closed because of an error. Only for when [`quit_app`](#quit_app) didn't work, which it says. Never used on the Album. No parameters.

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
Record the Switch's screen and save it on the PC (defaulting to `~/Pictures/kickstand/gif-<timestamp>.gif`), with a JPEG contact sheet of up to 6 evenly spaced frames so the agent can inspect the motion directly.
- **Gameplay video:** when the running game records gameplay (holding Capture in it saves a clip), or homebrew runs in such a game's place, and `ffmpeg` is installed on the PC, Kickstand streams the console's own 30 fps 1280x720 H.264 from the gameplay recorder (`grc:d`) and saves it as an `.mp4` (the video stream copied, no re-encode, with the game's sound as AAC when audio capture is running), then decodes the GIF frames from it. Nothing is buffered on the console beyond a small queue.
- **Otherwise** it falls back to repeated screenshots (a few frames a second) and says why.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `local_path` | `string` | `~/Pictures/kickstand/gif-<ts>.gif` | Path on the PC to save the GIF (the `.mp4` goes next to it). |
| `seconds` | `number` | `3.0` *(max 30)* | Duration to record in seconds. |
| `fps` | `integer` | `5` *(max 10)* | Frames per second in the GIF. |
| `width` | `integer` | `640` | Width of the GIF in pixels (`64`–`1280`). |

---

### `audio_replay`
The last few seconds (up to 5) of the running game's or homebrew's sound, after the fact. The sysmodule reads the foreground application's sound from the console's gameplay recorder (`grc:d`, 48 kHz 16-bit stereo) in the background into a 6-second ring, so this returns what already played. Saved as a `.wav` on the PC and analysed (RMS/peak, dominant frequency, silence, clipping, pops); `reference_wav` finds where a sound effect occurs in it (normalized cross-correlation).
- **What can be heard:** games that allow video capture, and homebrew running in place of such a game (title takeover). If the host game's control data says *Manual*, the homebrew must opt in by calling `appletInitializeGamePlayRecording()` (libnx) at startup. When nothing can be heard, the reply says why (no application, recording off, SysDVR running, ...) and what to do.
- **Applets and restarts:** an applet over the game (the home menu, Controllers) pauses the recorder until it closes. If Kickstand's background service is stopped while it waits on the recorder (e.g. the Kickstand NRO started from hbmenu over a game), the recorder sends nothing more until a reboot; the reply then says to reboot. `sysmodule install` and `update` pause capture before restarting it, so they don't cause this.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `seconds` | `number` | `5.0` *(0.1-5)* | How much recent audio to fetch. |
| `local_path` | `string` | `~/Pictures/kickstand/wav-<ts>.wav` | Where to save the `.wav`. |
| `reference_wav` | `string` | — | A `.wav` on the PC (an SFX) to look for in the capture. |

### `record_audio`
The next N seconds of the same sound, streamed live as it plays (same limits as `audio_replay`). Gaps (if the stream fell behind) are filled with silence and reported.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `seconds` | `number` | `3.0` *(0.1-30)* | How long to record. |
| `local_path` | `string` | `~/Pictures/kickstand/wav-<ts>.wav` | Where to save the `.wav`. |
| `reference_wav` | `string` | — | A `.wav` on the PC to look for in the recording. |

### `audio_control`
Query or set the system output volume (0-100%) and mute; also says whether Kickstand is hearing the running application.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `volume` | `integer` | — | New volume, 0-100. |
| `muted` | `boolean` | — | Mute or unmute. |


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
List the macros saved on this PC: controller input the user recorded once (in the desktop app's **Macros** page, with `kickstand macro record`, or through [`record_macro`](#record_macro)), or steps an agent wrote with [`save_macro`](#save_macro), for agents to replay with [`play_macro`](#play_macro), e.g. to get from the title screen to a level. Each comes with its length, its note (what it does and where it starts) and, for written ones, who wrote it. No parameters.

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

### `save_macro`
Save a macro the agent wrote itself, in [`input`](#input)'s step syntax, so it (another agent, or the user from the desktop app) can replay it later with [`play_macro`](#play_macro). The usual way: work the steps out with `input`, checking the screen, then save the same steps with a note saying where it starts. Give loading screens generous waits (`-:3000`), since load times vary. The steps are checked before saving; nothing is sent to the console. It won't replace an existing macro unless `replace` is true (and an agent should only replace one the user recorded if they agree). The desktop app shows who wrote each macro.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `name` | `string` *(required)* | — | Its name (letters, digits, spaces, `-` and `_`). |
| `steps` | `string` *(required)* | — | The input, e.g. `"a:100 -:2500 l=0,1:1800 a:100"`; up to 10 minutes in all. |
| `note` | `string` *(required)* | — | Where it starts and what it does. |
| `replace` | `boolean` | `false` | Replace a macro already called `name`. |

---

### `play_macro`
Replay a saved macro on a virtual controller with its recorded timing, e.g. to get back to the same place in a game before a test. Start from where it was recorded (its note says where). Timing can drift a little over a long macro, so check where it ended (`then: "still"`) and fix up with [`press`](#press) or [`input_until`](#input_until). Long macros are streamed to the console in pieces, without gaps.

While it plays, the user can pause or stop it from the desktop app (**Pause**/**Resume** and **Stop** appear on the Home and Macros pages) or with `kickstand macro pause|resume|stop`, and so can [`control_macro`](#control_macro). The result says if it was stopped part way and after how long.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `name` | `string` *(required)* | — | A saved macro's name (see [`macros`](#macros)). |
| `then` | `string` | — | `"still"` or `"change"`: return the screen afterwards, as in [`press`](#press). |
| `width` | `integer` | `640` | Width of that screenshot. |

---

### `control_macro`
Pause, resume or stop the macro playing on the console now, whoever plays it: [`play_macro`](#play_macro) (this agent's or another's), the desktop app, or a terminal. Paused, every button and stick is let go and the macro's clock stops; resumed, it carries on from the middle of the step it was in. A pause longer than 10 minutes stops the macro. Stopping lets go of everything at once, mid-step, and the rest of the macro is dropped. [`status`](#status) says when a macro is playing or paused.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `action` | `string` *(required)* | — | `"pause"`, `"resume"` or `"stop"`. |

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

> **Access control:** Kickstand's own folders, `/atmosphere/crash_reports`, `/atmosphere/fatal_reports`, and project destination folders from `projects.json` are accessible automatically, and so are the SD card paths an NRO is started with (`--log=sdmc:/game.log` in `run_nro` or a project's `args`), for reading. For any other SD card folder, Kickstand prompts the user in the AI app (*"Just this once"*, *"Until Kickstand restarts"*, or *"From now on"*; a file at the card's root is asked about by itself, not the whole card), or in its own desktop window when the AI app can't show the prompt (the agent asks in the chat at the same time, and passes the answer on with [`answer_access`](#answer_access), which takes the question out of the window), or the user can pre-authorize folders in a terminal with `kickstand allow <folder> [--write]`. System folders (`/atmosphere` outside crash reports, `/bootloader`, `/Nintendo`, keys, and NAND backups) are hardware-blocked by the daemon.

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
Upload a file from the PC to the Switch's SD card the fastest way available: straight to the SD card when it's mounted on the PC, over USB for files of 4 MB or more when a cable is plugged in (see [`usb_upload`](#usb_upload) for the one-time setup), else over Wi-Fi. If USB fails, the file goes over Wi-Fi and the result says why; automatic uploads then stay on Wi-Fi for 5 minutes. Streamed from disk in chunks and verified with CRC-32 on the console before replacing the target file. [`run_nro`](#run_nro) and [`reload_nro`](#reload_nro) choose the same way: a large build is copied over USB first and started from the SD card.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `local_path` | `string` *(required)* | — | Path to the source file on the PC. |
| `remote_path` | `string` *(required)* | — | Destination path on the SD card (e.g. `/switch/mygame/mygame.nro`). |

---

### `usb_upload`
Upload a file from the PC to the Switch's SD card over a USB cable, streamed from disk. Preferred over [`upload`](#upload) for large files like ROMs, ISOs or `.o2r` archives whenever [`status`](#status) reports a USB cable.
- **Kickstand's own USB interface:** for the transfer, the sysmodule presents a USB device on the console's USB-C port (`057e:3000`, named "Kickstand"), and the PC talks Kickstand's usual protocol over it with [nusb](https://github.com/kevinmehall/nusb), the same on Linux, macOS and Windows. It's the same upload as over Wi-Fi (staged, CRC-32 checked before it replaces the old file), so nothing is opened or closed on the console and a running game keeps running. The interface goes away when the upload ends (or after a minute without requests); while it's up, other USB homebrew (haze, DBI, Goldleaf) can't use the port. The cable has to go straight to the console, not through the dock.
- **A mounted SD card** (hekate's USB mass storage, a card reader, or `$KICKSTAND_USB_MOUNT`): copied straight to the card, staged in `/kickstand/staging/` and renamed into place.
- One-time setup on the PC: on Linux a udev rule, so your user may open the device; on Windows the WinUSB driver for it. The error says exactly what to do; see [USB transfers](README.md#usb-transfers).
- Same path and folder permission rules as [`upload`](#upload).
- **When this PC isn't set up for USB yet**, the upload goes over Wi-Fi (or `usb_upload` fails) and Kickstand asks the user, as a question in the AI app: *Set it up now* (starts `kickstand usb-setup`; their system asks them to approve it), *Use Wi-Fi for now*, or *Don't ask again*. Asked once each time Kickstand starts, after any tool whose upload needed it. In an AI app that can't show the question, the agent asks in the chat and the user runs the setup themselves; *Don't ask again* goes through [`answer_hint`](#answer_hint) with `hint: "usb_setup"`.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `local_path` | `string` *(required)* | — | Path to the source file on the PC. |
| `remote_path` | `string` *(required)* | — | Destination path on the SD card (e.g. `/switch/paperboat/paperboat.o2r`). |

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

### `answer_hint`
Pass on the user's answer to one of Kickstand's suggestions, the [libkickstand question](#reload_nro) or the [USB setup](#usb_upload), when a tool asked you to put it to them in the chat (AI apps that can't show Kickstand's question). Only what the user chose.

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `hint` | `string` | `"libkickstand"` | Which suggestion: `libkickstand` or `usb_setup`. |
| `app` | `string` | — | For `libkickstand`: the app the question was about, as the tool named it. |
| `answer` | `string` *(required)* | — | `libkickstand`: `add` (yes, add it to the build: the result says how), `not_now`, or `never` (don't ask again for this app). `usb_setup`: `not_now` or `never` (the user runs the setup themselves). |

---

## 6. Console & Daemon Management

Tools for checking console state, managing the background sysmodule, keeping the console awake, and warm-rebooting into Hekate.

### `status`
One-call snapshot of the console's state:
- Running application (game, or `hbloader` with `hbmenu`/NRO in full-memory mode) and running applet (`hbloader` in the Album).
- Whether the console is stuck on an Atmosphère fatal error screen.
- Whether the sysmodule is still pressing `B` to dismiss the lock screen after a reboot.
- Whether the Switch is connected to this PC via USB (or has its SD card mounted) so [`usb_upload`](#usb_upload) can be used for fast large-file transfers.
- Remaining keep-awake lease time, console uptime, any other AI agents currently sharing the console, a macro playing or paused (and who plays it), and a small screenshot (if the console refuses one, the rest is still returned with a note).

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

**For scripts, add `--json`** to any command that finishes with a result. It prints one JSON object: `{"ok": true, ...}` with the command's fields (and `"notes"` for anything it reported along the way), or `{"ok": false, "error": "..."}` with exit code 1. `run` and `tail -f` print JSON Lines instead, one object per event as it happens (`{"line": ..., "at": 1.234}` for each line of an NRO's stdout). Title and program ids are 16-digit hex strings. `serve`, `mock` and `watch` don't take it.

```sh
kickstand status --json | jq .application
kickstand grep --json -i error /kickstand/logs | jq -r '.matches[] | "\(.path):\(.line)"'
```

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
| | `kickstand reload <nro> [--hook\|--no-hook] [-- <args>...]` | Swap a new build in for the running homebrew without the Home Menu (the app's next-load hook, else hbmenu, else a full relaunch), print the path taken and time per phase, then stream stdout. |
| | `kickstand supervise` | Watch the running homebrew until it exits, crashes, fatals, or hangs. |
| | `kickstand hbmenu [--full]` | Open `hbmenu` in applet mode or `--full` title-takeover mode. |
| | `kickstand apps [--max <n>]` | List installed titles in home-menu order. |
| | `kickstand launch <title> [--user <n>]` | Launch an installed title by ID or name substring. |
| | `kickstand quit [--applet]` | Quit the running application from the home menu (HOME, Close software), or (`--applet`) close the Album. |
| | `kickstand force-quit` | Terminate the running application at once (it can't save). |
| | `kickstand app` | Show the currently running application and whether it is `hbloader`. |
| **See & Play** | `kickstand watch [--bind 0.0.0.0]` | Serve a live web page (`http://127.0.0.1:8790`) streaming the console's screen and status. |
| | `kickstand screenshot <file.jpg>` | Save a `1280x720` JPEG screenshot of the console. |
| | `kickstand wait [change\|still]` | Wait until the screen (or `--region`) changes or stops changing. |
| | `kickstand gif <file.gif>` | Record the screen as a looping GIF. |
| | `kickstand press <buttons>` | Press buttons on a virtual controller (or `unlock` to press `B` three times). |
| | `kickstand input <steps>` | Play timed `WHAT:MS` steps (buttons, sticks, touch). |
| | `kickstand type <text> [--enter]` | Type text on a virtual USB keyboard (into the software keyboard, for one). |
| | `kickstand macro record <name> [--note <text>] [--seconds <n>]` | Record the controllers while you play, until Enter is pressed in the terminal, and save it as a macro. |
| | `kickstand macro save <name> <steps> [--note <text>] [--replace]` | Save `input` steps as a macro. |
| | `kickstand macro play <name>` | Replay a saved macro. Ctrl-C stops it and lets go of everything. |
| | `kickstand macro pause` / `resume` / `stop` | Pause, resume or stop the macro playing now, whoever plays it (an agent, the desktop app or another terminal). |
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
| | `kickstand put [--usb\|--wifi] <local> <remote>` | Upload a file from the PC to the SD card (CRC-32 verified) the fastest way available: a mounted SD card, USB for files of 4 MB or more when a cable is in, else Wi-Fi. `--usb` or `--wifi` insists on one. |
| | `kickstand usb-put <local> <remote>` | Upload a file from the PC to the SD card over a USB cable (Kickstand's own USB interface, or a mounted SD card), for large files. |
| | `kickstand usb-setup` | One-time setup so this PC may use USB with the Switch: a udev rule on Linux, the WinUSB driver on Windows (as Zadig would). Says what it changes, then the system asks for approval. For the user to run, not an agent. |
| | `kickstand tail [-f] [-n <lines>] <path>` | Print the last lines of an SD card file (and follow with `-f`). |
| | `kickstand find <dir> [name] [--max <n>]` | Find files and folders by name, searched on the console. |
| | `kickstand grep [-i] <text> <path> [--max <n>]` | Print matching lines of a file or of every file under a folder, searched on the console. |
| | `kickstand rm [-r] <path>` | Delete a file or directory on the SD card. |
| **Console** | `kickstand status` | Show uptime, unlock state, keep-awake timer, running app, applet status, and USB connection status. |
| | `kickstand ping` | Ping the daemon and report round-trip time and version. |
| | `kickstand resources` | Show kernel resource limit usage of the daemon's group. |
| | `kickstand keep-awake [minutes]` | Set or check the keep-awake timer (`0` to end immediately). |
| | `kickstand sysmodule <subcommand>` | `status`, `install <nsp>`, `start`, `stop`, `handover`, `enable`, or `disable`. |
| | `kickstand reboot [target] [--yes]` | Warm-reboot into Hekate (`menu`, `ums`, or an entry name/number). |
| | `kickstand exit` | Stop the NRO daemon and return to `hbmenu`. |
