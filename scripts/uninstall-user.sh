#!/usr/bin/env bash
set -euo pipefail

systemctl --user disable --now voxtyped.service 2>/dev/null || true
systemctl --user disable --now voxtype-tray.service 2>/dev/null || true
rm -f \
  "$HOME/.local/bin/voxtype" \
  "$HOME/.local/bin/voxtyped" \
  "$HOME/.local/bin/voxtype-tray" \
  "$HOME/.local/bin/voxtype-overlay" \
  "$HOME/.local/bin/voxtype-settings" \
  "$HOME/.local/bin/voxtype-cleanup" \
  "$HOME/.local/share/voxtype/Overlay.qml" \
  "$HOME/.local/share/voxtype/Settings.qml" \
  "$HOME/.local/share/voxtype/Cleanup.qml" \
  "$HOME/.local/share/applications/io.github.tinnci.VoxType.desktop" \
  "$HOME/.local/share/applications/io.github.tinnci.VoxType.Settings.desktop" \
  "$HOME/.local/share/applications/io.github.tinnci.VoxType.Grammar.desktop" \
  "$HOME/.local/share/dbus-1/services/io.github.tinnci.VoxType.service" \
  "$HOME/.config/systemd/user/voxtyped.service" \
  "$HOME/.config/systemd/user/voxtype-tray.service"
systemctl --user daemon-reload
kbuildsycoca6 --noincremental
printf 'Uninstalled VoxType user integration\n'
