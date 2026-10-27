#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Toolchain check: build hello.nro, send it to hbmenu's NetLoader (press Y in hbmenu first) and print what
# it says. The NetLoader saves it as sdmc:/switch/kickstand/hello.nro (the folder must exist: run the daemon once).
#
#   scripts/run-hello.sh [switch-ip]
set -euo pipefail
source "$(dirname "$0")/common.sh"
IP="$(switch_ip "${1:-}")"
build_console hello
echo ">> sending hello.nro to $IP"
stdbuf -oL -eL "$NXLINK" -a "$IP" -r 3 -p kickstand/hello.nro --args "one two" -s "$NRO_DIR/hello.nro"
