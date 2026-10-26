# switch-mcp

Lets an AI agent develop and test on a real Nintendo Switch (Atmosphère) without anyone holding it:
deploy builds, launch and force-close them, reboot, take screenshots, press buttons, read memory and
fetch logs and crash reports.

All Rust:
- `daemon/`: runs on the Switch. Builds as a sysmodule (`daemon/sysmodule`) and as an hbmenu NRO
  (`daemon/app`) for testing without installing anything. Nightly, built with `cargo nx`.
- `mcp/`: the MCP server on the PC (stdio, on `rmcp`), plus a small CLI. Stable Rust.
- `proto/`: the wire protocol both sides share (no_std + serde/postcard).

Status: daemon v0 (ping, list/stat/read files, verified uploads) built, tested against a mock daemon, not
yet on hardware. See `context/DESIGN.md` and `context/AGENT_LOG.md`.

## Build
```sh
cargo build --release                                   # PC side: target/release/switch-mcp
cd daemon && cargo nx build --release -p switch-mcp-daemon   # console side (needs cargo-nx, see AGENT_LOG)
```

## Use
```sh
target/release/switch-mcp token                  # create the shared token (~/.config/switch-mcp/token)
echo 192.168.x.y > ~/.config/switch-mcp/host     # or pass --host / set SWITCH_IP
scripts/run-daemon-nro.sh                        # start the daemon NRO through hbmenu's NetLoader (press Y first)
target/release/switch-mcp ping                   # also: ls, cat, get, put
claude mcp add switch -- "$PWD/target/release/switch-mcp" serve
```
MCP tools: `switch_ping`, `switch_list_dir`, `switch_stat`, `switch_read_file`, `switch_download`,
`switch_upload`. Writes are limited to `/switch/paperboat/`, `/switch/ttydrecomp/` and `/switch-mcp/`; keys,
NAND backups and `/Nintendo` are never read.

## License
GPL-3.0-only (see `LICENSE`). `third_party/nx` is MIT.
