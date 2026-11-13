# Changelog

All notable changes to Kickstand are documented in this file.

## [0.1.2] - unreleased

### Added
- **Native USB Bulk Transport (`usb_upload`, `kickstand usb-put`, `kickstand put --usb`)**:
  - Added `services::usbds` (`usb:ds` 11.0+ wrapper) and `daemon_core::usb` so the background sysmodule presents a vendor-specific USB bulk interface (`057e:3000`, class `FF:4B:53`) on demand (`Request::UsbOpen` / `Request::UsbClose`, protocol 28) while games and NROs keep running.
  - Added pure-Rust PC USB transport via `nusb` (72 KiB page-aligned console buffers, 8 pipelined transfers in flight) plus automatic detection of mounted Switch SD cards (hekate UMS, card readers, or `$KICKSTAND_USB_MOUNT`) for high-speed streaming of large ROMs and `.o2r` archives.
  - Added `Client::upload_file` so Wi-Fi and USB uploads stream 64 KiB chunks directly from disk with incremental CRC-32 verification instead of buffering entire files in memory.
- **Modality-Aware Agent Sessions (`ping`, `modalities`)**:
  - Extended `ping` with optional `model` (`<provider>/<model>`) and `modalities` (`image`, `video`, `audio`; `text` is always baseline) parameters, validated by strict lowercase `^[a-z0-9][a-z0-9._-]*\/[a-z0-9][a-z0-9._:-]*$` naming.
  - Added a thread-safe per-session `ModelRegistry` (`Arc<RwLock<HashMap<String, Model>>>`) so visual and audio tools automatically tailor their output format to the calling model's capabilities.
  - Added `Modalities` bitflags (`IMAGE`, `VIDEO`, `AUDIO`) with `preferred_visual()` (`VIDEO` matched before `IMAGE`, then `TextSummary`) and `preferred_audio()` (`AUDIO` matched before `IMAGE` spectrogram, then `MetricsText`).
- **Gameplay Video Clips (`record_gif`, `kickstand gif`)** (protocol 30):
  - When the running game records gameplay (or homebrew runs in such a game's place), the sysmodule streams the console's own 30 fps 1280x720 H.264 from the gameplay recorder (`grc:d`) on demand (`Request::VideoStream`, a 384 KiB queue on the console that resumes at the next keyframe if the PC falls behind); the PC adds the parameter sets the recorder leaves out, muxes an `.mp4` (`ffmpeg -c:v copy`, frames numbered at the clip's rate, with the sound pulled from `Request::AudioStream` alongside as AAC) and decodes the GIF and contact sheet from it. Without ffmpeg, or when the game doesn't record, it falls back to screenshots and says why.
  - The PC also keeps a rolling ring of recent screenshots (`VideoRingBuffer`; `/clip` and `/ring` on `kickstand watch`). That ring is screenshots taken by the PC, not a stream from the console.
- **Fast In-Process NRO Hot-Reload (`reload_nro`, `kickstand reload`)**:
  - Swaps a new build in for the running homebrew inside the same hbloader process, without a Home Menu round trip, and reports the path taken (`hook`, `hbmenu` or `cold`) with the time for each phase (upload, exit, load).
  - **Next-load hook** (opt-in, a few lines of C or Rust, no library; TOOLS.md has both): Kickstand writes `/kickstand/next-load` (the NRO path, then its argv) and presses the exit button(s); an app started from a PC over nxlink deletes the file and calls `envSetNextLoad`, so hbloader loads the new build directly. The delete decides: if the file is still there 15 s later, Kickstand withdraws it, so a leftover request can't redirect a launch from hbmenu. Support is detected from the NRO itself (`--hook` / `--no-hook` override).
  - **libkickstand** (`libkickstand/`, a C static library for libnx homebrew; link with `--wrap` flags, nothing to call): `appletMainLoop()` returns false when Kickstand asks for a reload, so the app quits with no button press (`Hook::Ask`, detected by the `kickstand-lib/1` marker); it claims the request at `exit()`, and releases what the build leaves behind (unclosed thread stacks, threads still running are paused, JIT code memory, transfer memory) so the next build in the same process doesn't crash in `malloc`. Logs to `sdmc:/kickstand/libkickstand.log`. Verified with Paperboat and Uprising NX.
  - **libkickstand for Rust** (`libkickstand/rust`, crate `kickstand-nx`): for homebrew on the nx crate, which `--wrap` can't reach. `quit_requested()`, `spawn()` (tracked threads) and `finish()` (claims the request, pauses tracked threads still running after 1 s); same marker. hello.nro uses it.
  - **The libkickstand question** (`answer_hint`): a reload of a build without libkickstand asks the user, as a question with options in the AI app (MCP elicitation), whether the agent should add it (*Yes, add it to the build* / *Not now* / *Don't ask again for this app*, kept in `hints.json`); a yes returns the steps for libnx or nx, told apart from the NRO. Without question support, the agent is asked to put it to the user with its own question tool. `supervise` asks too when a build crashes within 2 minutes of an in-process reload without the library. Reloads end the old build by what the *running* build implements (recorded by `run_nro` and reloads), so the first build with the library still presses `+`.
  - Reloading onto the NRO that's running uploads to `<name>.reload.nro` beside it (the running build holds its own file open, 2002-0007), alternating from then on; that error no longer makes an automatic upload fall back to Wi-Fi and stop using USB.
  - Otherwise it exits to hbmenu, starts NetLoader (Y repeated every second) and chain-loads the build through `kickstand.nro --open`; if hbloader isn't running, a full `run_nro` launch. `exit_buttons` for apps that don't quit on `+`, and an optional macro replay afterwards.
- **Sampling CPU Profiler & Interactive SVG Flamegraphs (`profile`, `kickstand profile`)**:
  - Added `Request::Profile` / `Response::Profile` (protocol 29) in `daemon-core` to sample the running application's (or applet's) threads at 50-100 Hz via `svcDebugActiveProcess` and `svcBreakDebugProcess`, unwinding AArch64 frame-pointer chains (`fp` / `x29`) with leaf `lr` (`x30`) recovery while bounding sysmodule heap allocations to 32 KiB.
  - Added PC-side DWARF inline-aware symbolization, Brendan Gregg folded stack generation, zero-dependency interactive `.svg` flamegraph rendering (with zoom, hover tooltips, and regex search), and token-efficient AI reports ranking exclusive self-time hotspots and pruned inclusive call trees.
  - Added automatic fallback to PC-driven `Request::Threads` sampling when connected to an older daemon.
- **Audio Capture, Replay, Control & DSP Analysis (`audio_replay`, `record_audio`, `audio_control`, `kickstand audio`)** (protocol 30):
  - A sysmodule thread reads the foreground application's sound from the console's gameplay recorder (`grc:d`, 48 kHz 16-bit stereo) into a 6-second ring while an application runs (one `grc:d` and one `pm:dmnt` session for its life; idle otherwise). `audio_replay` returns the last 0.1-5 s of it after the fact; `record_audio` streams up to 30 s live by stream position (`Request::AudioStream`), filling and reporting gaps.
  - Only games that allow video capture can be heard, and homebrew running in such a game's place; homebrew under a host whose recording is *Manual* must call `appletInitializeGamePlayRecording()`. `Request::AudioCapture` reports the state (no application, waiting with the title's `VideoCapture` setting, capturing, SysDVR running, a failed call), and the tools turn it into what to do instead of returning silence.
  - `grc:d`'s stream is started at most once per boot, only after a Transfer answers "not initialized" (a second start would crash grc), and Kickstand stays off it while SysDVR runs. Nothing opens `audout:u`/`audin:u` and the debugger isn't involved.
  - A reader stopped in the middle of a `grc:d` Transfer (one blocks while the game is behind an applet such as the Album) leaves grc sending nothing to anyone until a reboot (found on 23.0.1). `Request::CaptureHold` pauses capture and waits for Transfers under way to finish, and `sysmodule install`/`update` send it before restarting the sysmodule (verified: capture continues across the restart). When a daemon was stopped without that, the next one reports `CaptureState::NeedsReboot` instead of "waiting", and the tools say to reboot.
  - PC side: WAV encoding/decoding, DSP metrics (RMS/peak dBFS, dominant frequency, silence, clipping, pops), reference `.wav` matching by exact normalized cross-correlation over every lag (FFT), ignoring windows far quieter than the reference, and waveform + spectrogram rendering.
- **USB First, Automatically (`upload`, `kickstand put`, `run_nro`, `reload_nro`)**:
  - Files go to a mounted SD card when there is one, else over USB when a cable is plugged in and the file is 4 MiB or more, else over Wi-Fi. A failed USB attempt falls back to Wi-Fi with the reason and keeps automatic uploads on Wi-Fi for 5 minutes. `run_nro` copies a large build over USB and starts it from the SD card instead of sending it to NetLoader over Wi-Fi. `put --usb` / `--wifi` and `usb_upload` insist on one.
- **One-Time USB Setup (`kickstand usb-setup`, desktop app's Set up USB)**:
  - When an upload couldn't use USB for want of it, Kickstand asks the user in the AI app (MCP elicitation): *Set it up now* (runs the setup; the system's own prompt approves it), *Use Wi-Fi for now*, *Don't ask again* (kept in `hints.json`). Without question support, the agent asks in the chat and the user runs `kickstand usb-setup` themselves.
  - Linux: a udev rule for `057e:3000` through the desktop's password prompt (`pkexec`) or `sudo`. Windows: the WinUSB driver, installed with libwdi (the library Zadig is built on) after Windows' admin prompt. macOS: nothing to do. The desktop app explains what changes in its own dialog first. CI runs the real setup on Linux, Windows and macOS runners.
- **Wireless Daemon Update from Local Builds (`update`)**:
  - Updated `kickstand update --switch` and the desktop app's **Update** button to detect local `cargo nx build --release` artifacts (`sys-kickstand.nsp` and `kickstand.nro`) when running a source build, and to offer a wireless Switch update whenever either the daemon crate version or `proto::PROTOCOL_VERSION` is behind the PC.

### Fixed
- **Uploads over USB are 3x faster: 19 MB/s, from 5.9** (64 MB, measured on hardware; Wi-Fi 7.0 MB/s, from 5.0). The console, not the link, was the limit, and the daemon now logs where an upload's time goes:
  - CRC-32 was computed a bit at a time, most of each chunk's time on the console's 1 GHz core; it now uses the CPU's CRC32 instructions there (34 ms per 64 MB) and slice-by-8 tables on the PC.
  - Byte payloads (`UploadChunk`, `Data`, audio and video chunks, `WriteMemory`) decoded one byte at a time through serde's sequence path; they're bytes now, written the same on the wire, so older daemons and clients still understand them.
  - 64 KiB SD writes ran at 13 MB/s: chunks are gathered into 512 KiB writes (about 20 MB/s), done on a writer thread of the sysmodule's own while the next chunks arrive, and the USB receive for the next request is posted before the current one is handled.
- **A freshly started Kickstand reloads from hbmenu without a 15 s wait**: it didn't know whether hbmenu or an NRO was showing, so it pressed `+` into hbmenu and waited for a next-load claim that never came (17.8 s). It now asks the console: the NRO loaded in hbloader is hbmenu if its header matches `sdmc:/hbmenu.nro`'s (2.9 s).
- **Opening hbmenu in applet mode no longer pushes buttons into a suspended app**: after Kickstand closed the Album (as every sysmodule install does), the home menu was left showing, and the next open pressed HOME, which resumes the suspended application, so Down, Right and A went to hbmenu (launching its fourth entry) or to whatever ran. Kickstand now remembers that it left the home menu showing, until anything changes the screen.
- **Holding R opens hbmenu again after Kickstand ran** (`third_party/nx` `input::Context`, `daemon_core::pm_input`):
  - The vendored nx deactivated npad for applet resource user id 0 whenever an input context was dropped (Kickstand's NRO screen, opened by every network sysmodule install, and macro recording). Atmosphère's pm reads held keys through that id, so holding R started the game instead of hbloader until a reboot, while Kickstand's own override still worked. nx now deactivates only its own id, and the sysmodule re-applies pm's settings at start, which mends an affected console without a reboot (verified on hardware).
- **Kickstand's NRO hands back to the sysmodule when a client ends it** (`kickstand exit`), not only on `+`; ending it left nothing listening until someone pressed `+` at the console.
- **`reload_nro` times its phases from the start of the exit press** (which takes about 1.4 s, attaching the virtual controller); it measured from the end, so the hook path read "exit 0.01 s, load 0.00 s" for a 1.9 s reload. The hook path now reports press, exit and load as one figure, since they overlap.
- **The desktop app counts Claude Code's agents when it was started from a launcher**: it looked for `claude` only on PATH, which a desktop launcher's PATH often lacks (`~/.local/bin`, where Claude Code's installer puts it), so Claude Code showed as not installed and its sessions weren't counted. Linking and unlinking Claude Code had the same problem.
- **nx's `hbl::get_loader_info()` stops at the NUL**: it returned hbloader's hidden quotation after the version.
- **A forgotten hbl override is disarmed after 60 s**: a client killed mid-launch left the title starting as hbloader unless R was held.
- **Cancelled Transfer Recovery (`mcp/src/client.rs`, `daemon/core/src/server.rs`)**:
  - Replaced client connections left with a half-sent frame or unread replies after a cancelled tool call, timed out daemon frame reads stalled for 30 seconds (`FRAME_STALL_MS`), and cleaned up leftover `upload-*.part` staging files on daemon startup.
- **USB Transport Lifecycle (`daemon/core/src/usb.rs`, `mcp/src/usb.rs`)**:
  - Fixed a close/reopen race in `daemon_core::usb` by tracking `busy` across the full `usb:ds` device teardown (`drop(device)`) and waiting for `Dir::In` reply completion before closing the session on `Request::UsbClose`.
  - Added a per-process atomic sequence counter to mounted SD card staging filenames (`usb-<pid>-<seq>.part`) so concurrent transfers cannot collide.

---

## [0.1.1] - 2026-10-31

### Added
- **Windows Support (`x86_64-pc-windows-msvc`)**:
  - Added Windows configuration (`%APPDATA%\kickstand`) and cache (`%LOCALAPPDATA%\kickstand`) paths.
  - Updated self-updater to extract `kickstand.exe` from `.zip` release archives and atomically replace the running binary (`kickstand.old.exe`, cleaned up on next startup).
  - Detached console window when launching the desktop GUI on Windows (`hide_own_console`).
  - Added Windows release matrix and CI test workflow.

---

## [0.1.0] - 2026-10-31

### Added
- **Initial Public Release of Kickstand (`daemon`, `mcp`, `proto`)**:
  - Nintendo Switch background sysmodule (`sys-kickstand`, 8 MiB heap) and hbmenu NRO (`kickstand.nro`) written in nightly Rust on `nx`.
  - Unified PC binary (`kickstand`) providing an `egui` desktop app, a stdio MCP server (`kickstand serve`), and a full CLI (`kickstand <subcommand>`) with global `--json` output.
  - Full-memory homebrew execution (`run_nro`, `test_build`, `supervise`) via Atmosphère title takeover and `nxlink` stdout streaming with DWARF backtrace symbolization (`symbolize`, `threads`, `crash_report`, `crash_reports`).
  - Live process memory inspection and patching (`memory_map`, `read_memory`, `write_memory`) with symbolic expression evaluation.
  - Screen capture (`screenshot`, `wait_screen`, `record_gif`) and virtual Pro Controller / touch / keyboard automation (`press`, `input`, `input_until`, `type_text`, `macros`, `record_macro`, `save_macro`, `play_macro`, `control_macro`).
  - Console-side file management and search (`list_dir`, `stat`, `read_file`, `tail_file`, `find`, `grep`, `upload`, `download`, `delete`) with interactive folder access prompts (`answer_access`) and system path guardrails.
  - LAN UDP discovery (`28791`), 6-digit on-screen pairing (`pair`, `unpair`), session-based multi-agent activity attribution, Ed25519-signed self-updater (`update`), keep-awake management (`keep_awake`), and warm reboot into hekate (`reboot`, `hekate_entries`).
