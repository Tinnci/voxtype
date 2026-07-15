#!/usr/bin/env bash
set -euo pipefail

systemctl --user disable --now voxtyped.service 2>/dev/null || true
rm -f \
  "$HOME/.local/bin/voxtype" \
  "$HOME/.local/bin/voxtyped" \
  "$HOME/.local/share/applications/io.github.tinnci.VoxType.desktop" \
  "$HOME/.local/share/dbus-1/services/io.github.tinnci.VoxType.service" \
  "$HOME/.config/systemd/user/voxtyped.service"
systemctl --user daemon-reload
kbuildsycoca6 --noincremental
printf 'Uninstalled VoxType user integration\n'
