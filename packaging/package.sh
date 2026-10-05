#!/bin/bash
# package.sh - build tgh-server and pack it: dist/tgh-server-<version>-linux-<arch>.tar.gz (+ .sha256).
#   packaging/package.sh                 build with cargo (needs zig for the native library) and pack
#   SKIP_BUILD=1 packaging/package.sh    pack what target/release already holds
# The tarball has `install.sh` (install / uninstall, PREFIX, DESTDIR), the binary, the dashboard launcher, an OpenRC
# service (/etc/init.d, /etc/conf.d) and a systemd user unit. Files are named one by one: the working .env and
# data/ in this checkout hold the owner's secrets and must never be packed.
set -euo pipefail
cd "$(dirname "$0")/.."
NAME=tgh-server
ID=io.github.tarilka0gg.TelegramHelper
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/server/Cargo.toml | head -1)
ARCH=$(uname -m)
[ "${SKIP_BUILD:-}" = 1 ] || cargo build --release --locked -p tgh-server

D=dist/$NAME-$VERSION
rm -rf "${D:?}"
install -Dm755 target/release/tgh-server -t "$D/prefix/bin"
install -Dm644 "packaging/$ID.desktop" -t "$D/prefix/share/applications"
install -Dm644 packaging/icons/$NAME.svg -t "$D/prefix/share/icons/hicolor/scalable/apps"
install -Dm644 .env.example packaging/tgh-server.service -t "$D/prefix/share/$NAME"
install -Dm644 README.md LICENSE SECURITY.md -t "$D/prefix/share/doc/$NAME"
install -Dm755 packaging/etc/init.d/tgh-server "$D/etc/init.d/tgh-server"
install -Dm644 packaging/etc/conf.d/tgh-server "$D/etc/conf.d/tgh-server"
install -m755 packaging/install.sh "$D/install.sh"
cat > "$D/POST-INSTALL.txt" <<'TXT'
tgh-server reads .env and data/ from its working directory (see /usr/local/share/tgh-server/.env.example).
  per user:  mkdir -p ~/.local/share/tgh-server; put .env there; run `cd ~/.local/share/tgh-server && tgh-server`,
             or use the systemd user unit in /usr/local/share/tgh-server/. `tgh-server --demo` needs no credentials.
  OpenRC:    useradd -r -m -d /var/lib/tgh-server tgh; put .env in /var/lib/tgh-server (owner tgh, mode 600);
             rc-update add tgh-server default; rc-service tgh-server start
The dashboard is at http://127.0.0.1:8787/.
TXT

OUT=dist/$NAME-$VERSION-linux-$ARCH.tar.gz
tar -C dist --owner=0 --group=0 -czf "$OUT" "$NAME-$VERSION"
(cd dist && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256")
echo "$OUT"
