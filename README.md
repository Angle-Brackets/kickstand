# Kickstand

Lets an AI agent develop and test on a real Nintendo Switch (Atmosphère) without anyone holding it:
deploy builds, launch and force-close them, reboot, take screenshots, press buttons, read memory and
fetch logs and crash reports.

All Rust:
- `daemon/`: runs on the Switch. Builds as a sysmodule (`daemon/sysmodule`) and as an hbmenu NRO
  (`daemon/app`) for testing without installing anything. Nightly, built with `cargo nx`.
- `mcp/`: the MCP server on the PC (stdio, on `rmcp`), plus a small CLI. Stable Rust.
- `proto/`: the wire protocol both sides share (no_std + serde/postcard).

Status: on hardware (fw 23.0.1): files (list, read, verified upload, delete) at ~2 MB/s over Wi-Fi, and the
sysmodule runs (started by hand; not at boot yet). See `context/DESIGN.md` and `context/AGENT_LOG.md`.

## Build
```sh
cargo build --release                                   # PC side: target/release/kickstand
cd daemon && cargo nx build --release -p kickstand-nro   # console side (needs cargo-nx, see AGENT_LOG)
```

## Use
```sh
target/release/kickstand token                  # create the shared token (~/.config/kickstand/token)
echo 192.168.x.y > ~/.config/kickstand/host     # or pass --host / set SWITCH_IP
scripts/run-daemon-nro.sh                        # start the daemon NRO through hbmenu's NetLoader (press Y first)
target/release/kickstand ping                   # also: ls, cat, get, put, rm, exit
target/release/kickstand sysmodule install daemon/target/aarch64-nintendo-switch-freestanding/release/sys-kickstand.nsp
target/release/kickstand sysmodule handover      # start the sysmodule; the NRO exits; no hbmenu needed after
claude mcp add kickstand -- "$PWD/target/release/kickstand" serve
```
MCP tools: `ping`, `list_dir`, `stat`, `read_file`, `download`, `upload`, `delete`, `sysmodule`
(status, install, start, stop, handover). The sysmodule never gets `boot2.flag` from these tools. Writes and deletes are limited to `/switch/paperboat/`, `/switch/ttydrecomp/` and `/kickstand/`; keys,
NAND backups and `/Nintendo` are never read.

## License
GPL-3.0-only (see `LICENSE`). `third_party/nx` is MIT.
