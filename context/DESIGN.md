# Design

## Why
Testing PaperBoat (and later TTYDRecomp) on hardware needs the user present: open the Homebrew Menu through a
game, press Y for the NetLoader, look at the screen, report back. A daemon on the console removes almost all
of that, so the agent can make a change, build, deploy, play to the bug, screenshot it, read the crash
report, fix it and repeat.

## Architecture
```
kickstand/
  daemon/   sysmodule on the `nx` crate: command server, launch/kill/reboot, screenshots, input, files, watchdog
  proto/    shared command and response types (no_std + serde), used by both sides
  mcp/      MCP server on rmcp: a thin client to the daemon over TCP
```
One protocol crate means the two halves can't drift apart.

## What the daemon replaces
| Gap today | Daemon |
| - | - |
| Needs hbmenu's NetLoader open | Receives builds itself, writes them to the SD card |
| A hung game needs the user | Separate process, survives the game; force-closes it (`pm:shell` TerminateProgram) |
| Recovering the console | Reboot (`bpc`). Should recover many fatal screens, not a kernel hang |
| Stuck console, or the SD card needed on the PC | Reboot to hekate (see below): boot a chosen hekate entry, or hekate's USB mass storage so the PC mounts the SD card with no one pulling it |
| Relaunching with full memory | Launch a title with Atmosphère's hbl override (hbl_config path can point at a specific .nro). **Least proven: prototype early.** Fallback: a small launcher app left open in hbmenu |
| Sleep drops Wi-Fi | Turn auto-sleep off during a session |
| Seeing and playing | Screenshots (`caps:sc`), virtual controller (`hid:dbg`), memory read/write (as sys-botbase does) |
| Logs, crash reports | Reads `sdmc:/atmosphere/crash_reports/` and app logs directly. No FTP module |
| Stepping through code | Later: Atmosphère's GDB stub (`enable_standalone_gdbstub`), driven from the MCP side |

Only the power button should ever need the user.

## Toolchain
- Target `aarch64-nintendo-switch-freestanding` (tier 3): nightly + `-Z build-std`. The nightly is pinned in
  `daemon/rust-toolchain.toml` (nightly-2026-09-08: newer ones break nx's `unwinding` dependency). The host
  side (`proto`, `mcp`) builds on stable.
- `nx` is vendored in `third_party/nx` with a small patch (argv, for nxlink); see its `PATCHES.md`.
  `context/NX_SERVICES.md` lists what nx covers and the wrappers we must write.
- [`nx`](https://github.com/aarch64-switch-rs/nx): pure-Rust libnx replacement (IPC, fs, sockets, hid,
  allocators; sysmodules and NROs). emuiibo is a Rust sysmodule on it, so use it as a reference.
- `cargo-nx` builds the .nsp/.nro.
- PC side: `rmcp` (official Rust MCP SDK).
- Expect to add IPC wrappers ourselves for services `nx` lacks (likely `caps:sc`, `hid:dbg`, `pm:shell`, `bpc`).
  libnx's source shows each command's id and layout.

## Constraints and risks
- **Memory:** sysmodules get a few MB. Small heap, no big crates, stream screenshots raw (encode on the PC).
- **Idle while games run:** no busy loops, so benchmarks aren't skewed.
- **Boot loops:** a sysmodule that crashes at boot can stop the console booting. Keep the boot2 flag off while
  developing and start it by hand. Recovery: take out the SD card, delete `atmosphere/contents/<id>/`.
- **Safety:** the console refuses what's never safe (writes in Atmosphère's, hekate's and Nintendo's
  folders except Kickstand's sysmodule, the top of the card, keys; reads of keys, NAND backups, `/Nintendo`).
  The PC asks the user (an MCP elicitation) before an agent uses any folder outside Kickstand's own, crash
  reports and the projects' folders (`mcp/src/access.rs`). No project names in the code: games are whatever
  `projects.json` lists. Memory writes only to the process being developed.
- **No auth on a LAN socket:** at least a shared token, and bind only while a dev session is enabled.

## Build order
1. Toolchain: nightly, build-std, a hello-world NRO on `nx` that runs on hardware.
2. Daemon v0: ping, list/read files, upload a build. MCP server exposing those.
   Done, on hardware. The sysmodule installs over the network and takes over from the NRO (`handover`),
   with boot2 off.
3. Launch / force-close / reboot. Prototype the hbl-override launch early. Reboot comes in two forms:
   - plain reboot through `bpc` (Atmosphère's bpc mitm may still route it to `reboot_payload.bin`);
   - **reboot to hekate**: read hekate's payload from the SD card, hand it to Atmosphère with
     `bpc:ams` SetRebootPayload, then reboot through `bpc`. Before handing it over, fill in hekate's boot
     config (the block hekate reads from its own payload) to pick what hekate does: boot a named
     `hekate_ipl.ini` entry (e.g. back into Atmosphère, emuMMC or sysMMC) or start USB mass storage of the
     SD card, so the PC can mount it. nx-agent does this ("Remote Hekate").
   Verify the boot config layout and payload size limit against hekate's and Atmosphère's sources first.
   hekate is GPL-2.0, which doesn't combine with GPL-3.0: take the layout as facts, write the code ourselves.
   A bad payload or boot config can leave the console on a black screen until someone holds the power
   button (and injects a payload, on an RCM setup), so gate this behind a confirm step and test it first with
   the user watching.
   Then turn on boot2 for the sysmodule (with the user's go-ahead) once it has run a while without trouble.
4. Screenshots and button input.
   Also a simple on-screen UI for the NRO daemon (user's request): IP and port, connected client, last
   requests, "press + to exit", using nx's text console. The black screen confused us once. A UI for the
   sysmodule would be an overlay (Tesla is C++; look for a Rust option).
5. Watchdog: detect a hung app (no heartbeat or log output) and recover.
6. GDB stub integration.
Steps 1–4 are the useful core: deploy, test, look, recover.

## Future ideas
- **More than one Switch** (not needed yet: the user has one). Today this PC keeps one address
  (`~/.config/kickstand/host`), and each MCP server reads it at startup. If it's ever wanted:
  - Remember every paired console by its System Settings nickname (discovery already announces it) and last
    address, and find it again on the network when the address changes.
  - Keep one default; add a `consoles` tool to list them and move a session to another by name.
  - Name the console in every reply ("Ankit's Switch: …"), and mention the others when the agent didn't
    pick one, so "ping my switch" stays unambiguous.
  - The desktop app shows a row per console, with its agents.
  - The default changes only when the user changes it, never when an agent moves to another console.
