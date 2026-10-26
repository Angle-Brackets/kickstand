#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Run the daemon as an NRO (no sysmodule install): build it, send it to hbmenu's NetLoader (press Y in
# hbmenu first) with the shared token as an argument, and stream its log. The NetLoader saves it as
# sdmc:/switch/switch-mcp-daemon.nro. Press + on the console to stop it.
#
#   scripts/run-daemon-nro.sh [switch-ip]
set -euo pipefail
source "$(dirname "$0")/common.sh"
IP="$(switch_ip "${1:-}")"
cargo build -q --release --manifest-path "$ROOT/Cargo.toml" -p switch-mcp
TOKEN="$("$ROOT/target/release/switch-mcp" token | sed -n 's/^token: //p')"
build_console switch-mcp-daemon
echo ">> sending switch-mcp-daemon.nro to $IP; then try: target/release/switch-mcp --host $IP ping"
stdbuf -oL -eL "$NXLINK" -a "$IP" -r 3 -p switch-mcp-daemon.nro --args "--token $TOKEN" -s \
    "$NRO_DIR/switch-mcp-daemon.nro"
