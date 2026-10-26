# SPDX-License-Identifier: GPL-3.0-only
# Sourced by the other scripts.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NXLINK="${DEVKITPRO:-/opt/devkitpro}/tools/bin/nxlink"
NRO_DIR="$ROOT/daemon/target/aarch64-nintendo-switch-freestanding/release"

# The Switch's IP: first argument, then $SWITCH_IP, then ~/.config/switch-mcp/host.
switch_ip() {
    local ip="${1:-${SWITCH_IP:-$(cat "${XDG_CONFIG_HOME:-$HOME/.config}/switch-mcp/host" 2>/dev/null || true)}}"
    if [[ -z "$ip" ]]; then
        echo "Pass the Switch's IP, set SWITCH_IP, or put it in ~/.config/switch-mcp/host." >&2
        exit 1
    fi
    echo "$ip"
}

build_console() {
    (cd "$ROOT/daemon" && cargo nx build --release -p "$1")
}
