# Changelog

All notable changes to Kickstand are documented in this file.

## [0.1.2] - 2026-11-11

### Added
- **Native USB Bulk Transport (`usb_upload`, `kickstand usb-put`, `kickstand put --usb`)**:
  - Added `services::usbds` (`usb:ds` 11.0+ wrapper) and `daemon_core::usb` so the background sysmodule presents a vendor-specific USB bulk interface (`057e:3000`, class `FF:4B:53`) on demand (`Request::UsbOpen` / `Request::UsbClose`, protocol 28) while games and NROs keep running.
  - Added pure-Rust PC USB transport via `nusb` (72 KiB page-aligned console buffers, 8 pipelined transfers in flight) plus automatic detection of mounted Switch SD cards (hekate UMS, card readers, or `$KICKSTAND_USB_MOUNT`) for high-speed streaming of large ROMs and `.o2r` archives.
  - Added `Client::upload_file` so Wi-Fi and USB uploads stream 64 KiB chunks directly from disk with incremental CRC-32 verification instead of buffering entire files in memory.
- **Modality-Aware Agent Sessions (`ping`, `modalities`)**:
  - Extended `ping` with optional `model` (`<provider>/<model>`) and `modalities` (`image`, `video`, `audio`; `text` is always baseline) parameters, validated by strict lowercase `^[a-z0-9][a-z0-9._-]*\/[a-z0-9][a-z0-9._:-]*$` naming.
  - Added a thread-safe per-session `ModelRegistry` (`Arc<RwLock<HashMap<String, Model>>>`) so visual and audio tools automatically tailor their output format to the calling model's capabilities.
  - Added `Modalities` bitflags (`IMAGE`, `VIDEO`, `AUDIO`) with `preferred_visual()` (`VIDEO` matched before `IMAGE`, then `TextSummary`) and `preferred_audio()` (`AUDIO` matched before `IMAGE` spectrogram, then `MetricsText`).
- **Streamed Video Capture & Rolling Frame Ring Buffer (`record_gif`, `watch`)**:
  - Added `VideoRingBuffer` on the PC (capped at 150 frames, 32 MiB, and a 30-second sliding window) fed by screen captures and the live browser viewer.
  - Added `export_clip` to encode captured screen sequences as `.mp4` (via `ffmpeg` when installed, with automatic `.gif` fallback), a 6-frame JPEG contact sheet, or a motion/frame-rate text summary depending on the session's registered modalities.
  - Added `/clip` and `/ring` endpoints to `kickstand watch` so developers and agents can inspect or download the rolling screen capture buffer on demand.
- **Fast In-Process NRO Hot-Reload (`reload_nro`, `kickstand reload`)**:
  - Added `reload_nro` MCP tool and `kickstand reload` CLI subcommand to hot-reload homebrew NROs without a 10-15 second Home Menu round-trip.
  - Stages the updated `.nro` to `/switch/...` over USB (`usb_upload`) or Wi-Fi while the current NRO is still running, exits the running NRO cleanly back to `hbmenu` inside the existing `hbloader` process, and chainloads the new NRO via `kickstand.nro --open <dest> [args...]`.
  - Automatically falls back to a full `run_nro` launch if `hbloader` is not running or does not answer on NetLoader, and supports optional `replay_macro` execution immediately after reload.
- **Sampling CPU Profiler & Interactive SVG Flamegraphs (`profile`, `kickstand profile`)**:
  - Added `Request::Profile` / `Response::Profile` (protocol 29) in `daemon-core` to sample the running application's (or applet's) threads at 50-100 Hz via `svcDebugActiveProcess` and `svcBreakDebugProcess`, unwinding AArch64 frame-pointer chains (`fp` / `x29`) with leaf `lr` (`x30`) recovery while bounding sysmodule heap allocations to 32 KiB.
  - Added PC-side DWARF inline-aware symbolization, Brendan Gregg folded stack generation, zero-dependency interactive `.svg` flamegraph rendering (with zoom, hover tooltips, and regex search), and token-efficient AI reports ranking exclusive self-time hotspots and pruned inclusive call trees.
  - Added automatic fallback to PC-driven `Request::Threads` sampling when connected to an older daemon.
- **Audio Capture, 5-Second Replay Buffer, Control & DSP Analysis (`audio_replay`, `record_audio`, `audio_control`, `kickstand audio`)**:
  - Added `services::audio` and `daemon_core::audio` with a background sampler maintaining a 5-second rolling 48 kHz 16-bit stereo PCM ring buffer (`960 KiB`) on the console via zero-handle-leak debug memory capture against the running application.
  - Added `audio_replay` to save and analyze the last 0.1-5.0 seconds of console audio retroactively, `record_audio` to stream up to 30 seconds of live audio, and `audio_control` to query or set system audio output volume (`0-100%`) and mute state.
  - Added pure-Rust PC-side WAV encoding/decoding, DSP waveform metrics (RMS dBFS, peak dBFS, DC offset, zero-crossing rate, dominant Goertzel frequency, silence/clipping/phase-discontinuity glitch detection), normalized cross-correlation against reference `.wav` files, and 800x400 RGB waveform + frequency spectrogram rendering.
- **Wireless Daemon Update from Local Builds (`update`)**:
  - Updated `kickstand update --switch` and the desktop app's **Update** button to detect local `cargo nx build --release` artifacts (`sys-kickstand.nsp` and `kickstand.nro`) when running a source build, and to offer a wireless Switch update whenever either the daemon crate version or `proto::PROTOCOL_VERSION` is behind the PC.

### Fixed
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
