#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bindir=${HOME}/.local/bin
applications_dir=${HOME}/.local/share/applications
dbus_services_dir=${HOME}/.local/share/dbus-1/services
systemd_dir=${HOME}/.config/systemd/user

for command in cargo install systemctl kbuildsycoca6 parec notify-send; do
  if ! command -v "$command" >/dev/null 2>&1; then
    printf 'Required command is missing: %s\n' "$command" >&2
    exit 1
  fi
done
if ! command -v qml6 >/dev/null 2>&1 \
  && [[ ! -x /usr/lib/qt6/bin/qml6 ]] \
  && [[ ! -x /usr/libexec/qt6/qml6 ]]; then
  printf 'Required Qt 6 QML runtime is missing (qml6).\n' >&2
  exit 1
fi
for command in curl secret-tool wl-copy wl-paste ydotool; do
  if ! command -v "$command" >/dev/null 2>&1; then
    printf 'Optional capability is unavailable; command missing: %s\n' "$command" >&2
  fi
done

cd "$repo_root"
cargo build --release --locked --workspace --bins

install -Dm755 target/release/voxtype "$bindir/voxtype"
install -Dm755 target/release/voxtyped "$bindir/voxtyped"
install -Dm755 target/release/voxtype-tray "$bindir/voxtype-tray"
install -Dm755 target/release/voxtype-overlay "$bindir/voxtype-overlay"
install -Dm755 target/release/voxtype-settings "$bindir/voxtype-settings"
install -Dm755 target/release/voxtype-cleanup "$bindir/voxtype-cleanup"
install -Dm644 packaging/qml/Overlay.qml "$HOME/.local/share/voxtype/Overlay.qml"
install -Dm644 packaging/qml/Settings.qml "$HOME/.local/share/voxtype/Settings.qml"
install -Dm644 packaging/qml/Cleanup.qml "$HOME/.local/share/voxtype/Cleanup.qml"
install -Dm644 packaging/applications/io.github.tinnci.VoxType.desktop \
  "$applications_dir/io.github.tinnci.VoxType.desktop"
install -Dm644 packaging/applications/io.github.tinnci.VoxType.Settings.desktop \
  "$applications_dir/io.github.tinnci.VoxType.Settings.desktop"
install -Dm644 packaging/applications/io.github.tinnci.VoxType.Grammar.desktop \
  "$applications_dir/io.github.tinnci.VoxType.Grammar.desktop"
install -Dm644 packaging/systemd/voxtyped.service \
  "$systemd_dir/voxtyped.service"
install -Dm644 packaging/systemd/voxtype-tray.service \
  "$systemd_dir/voxtype-tray.service"

mkdir -p "$dbus_services_dir"
sed "s|@BINDIR@|$bindir|g" \
  packaging/dbus/io.github.tinnci.VoxType.service.in \
  > "$dbus_services_dir/io.github.tinnci.VoxType.service"

systemctl --user daemon-reload
systemctl --user enable voxtyped.service
systemctl --user enable voxtype-tray.service
systemctl --user restart voxtyped.service
systemctl --user restart voxtype-tray.service
kbuildsycoca6 --noincremental

printf 'Installed VoxType for %s\n' "$USER"
printf 'Daemon status: '
"$bindir/voxtype" status
