#!/usr/bin/env bash
# Build (or take AFTERIMAGE_BINARY=<built binary>) and install afterimage for the current user:
# ~/.local/share/afterimage/bin/afterimage, a link in ~/.local/bin, the user service, an example config.
# AFTERIMAGE_NO_SERVICE=1 skips enabling the service (e.g. when your session starts it another way).
set -euo pipefail
src=$(cd "$(dirname "$0")" && pwd)
dest=${XDG_DATA_HOME:-$HOME/.local/share}/afterimage
conf=${XDG_CONFIG_HOME:-$HOME/.config}/afterimage
units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user

bin=${AFTERIMAGE_BINARY:-}
if [ -z "$bin" ]; then
    command -v cargo >/dev/null || { echo "error: cargo not found (or pass AFTERIMAGE_BINARY=<built binary>)" >&2; exit 1; }
    nice -n 10 cargo build --release --locked --manifest-path "$src/Cargo.toml"
    bin=$src/target/release/afterimage
fi
mkdir -p "$dest/bin" "$HOME/.local/bin" "$conf" "$units"
install -m 755 "$bin" "$dest/bin/.afterimage.new"
mv -f "$dest/bin/.afterimage.new" "$dest/bin/afterimage"   # atomic: a running daemon keeps its old inode
ln -sf "$dest/bin/afterimage" "$HOME/.local/bin/afterimage"
[ -f "$conf/config.toml" ] || cp "$src/config.example.toml" "$conf/config.toml"
cp "$src/systemd/afterimage.service" "$units/"
systemctl --user daemon-reload
if [ -z "${AFTERIMAGE_NO_SERVICE:-}" ]; then
    systemctl --user enable afterimage.service >/dev/null
    systemctl --user restart afterimage.service
fi
echo "installed: $("$dest/bin/afterimage" --version) → $dest/bin · config $conf/config.toml"
echo "try: afterimage status   afterimage search 'error 20 min ago'   afterimage ui"
