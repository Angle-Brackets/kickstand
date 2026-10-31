#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Sets the version of the PC app and the Switch side (they're released together), then commits and tags.
# Usage: scripts/bump-version.sh 0.1.0   (then: git push && git push origin v0.1.0)
set -euo pipefail
cd "$(dirname "$0")/.."
version="${1:?usage: $0 X.Y.Z}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: $version" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "commit or stash your changes first" >&2; exit 1; }
sed -i "0,/^version = \".*\"/s//version = \"$version\"/" mcp/Cargo.toml
sed -i "/\[workspace.package\]/,/^\[/s/^version = \".*\"/version = \"$version\"/" daemon/Cargo.toml
cargo update -q -w
(cd daemon && cargo update -q -w)
# Already at this version (the first release): just tag.
git diff --quiet || git commit -qam "Version $version"
git tag "v$version"
echo "tagged v$version: push with  git push && git push origin v$version"
