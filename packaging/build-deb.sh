#!/bin/sh
# Build a .deb from the release binaries:  cargo build --release && packaging/build-deb.sh
set -eu
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ROOT=$(mktemp -d)
chmod 755 "$ROOT"
trap 'rm -r "$ROOT"' EXIT
install -Dm755 target/release/agent-control-centre "$ROOT/usr/bin/agent-control-centre"
install -Dm755 target/release/acc "$ROOT/usr/bin/acc"
install -Dm644 packaging/agent-control-centre.desktop "$ROOT/usr/share/applications/agent-control-centre.desktop"
install -Dm644 LICENSE "$ROOT/usr/share/doc/agent-control-centre/copyright"
mkdir -p "$ROOT/DEBIAN"
cat > "$ROOT/DEBIAN/control" <<EOF
Package: agent-control-centre
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Depends: libc6, libgl1, libxkbcommon0
Maintainer: Agent Control Centre contributors
Homepage: https://github.com/muzammil5539/agent-control-centre
Description: Always-on-top HUD to monitor, approve and control AI agents
 A native desktop panel that shows every AI agent on this machine, asks you
 before they act, and keeps a history. Includes the acc command-line helper.
EOF
mkdir -p dist
dpkg-deb --build --root-owner-group "$ROOT" "dist/agent-control-centre_${VERSION}_amd64.deb"
