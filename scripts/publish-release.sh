#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Signs and publishes the draft release CI built for a tag. Installed copies only take a release whose
# SHA256SUMS carries a signature from the key in ~/.config/kickstand-release (kept off GitHub).
# Usage: scripts/publish-release.sh v0.2.0
set -euo pipefail
cd "$(dirname "$0")/.."
tag="${1:?usage: $0 vX.Y.Z}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
gh release download "$tag" --dir "$work"
(cd "$work" && sha256sum --check --strict SHA256SUMS)
cargo run -q -p kickstand-release -- sign "$work/SHA256SUMS"
gh release upload "$tag" "$work/SHA256SUMS.sig" --clobber
gh release edit "$tag" --draft=false --latest
echo "published $tag"
