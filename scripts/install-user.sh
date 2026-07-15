#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bindir=${HOME}/.local/bin
applications_dir=${HOME}/.local/share/applications
dbus_services_dir=${HOME}/.local/share/dbus-1/services
systemd_dir=${HOME}/.config/systemd/user

cd "$repo_root"
cargo build --release --workspace --bins

install -Dm755 target/release/voxtype "$bindir/voxtype"
install -Dm755 target/release/voxtyped "$bindir/voxtyped"
install -Dm644 packaging/applications/io.github.tinnci.VoxType.desktop \
  "$applications_dir/io.github.tinnci.VoxType.desktop"
install -Dm644 packaging/systemd/voxtyped.service \
  "$systemd_dir/voxtyped.service"

mkdir -p "$dbus_services_dir"
sed "s|@BINDIR@|$bindir|g" \
  packaging/dbus/io.github.tinnci.VoxType.service.in \
  > "$dbus_services_dir/io.github.tinnci.VoxType.service"

systemctl --user daemon-reload
systemctl --user enable --now voxtyped.service
kbuildsycoca6 --noincremental

printf 'Installed VoxType for %s\n' "$USER"
printf 'Daemon status: '
"$bindir/voxtype" status
