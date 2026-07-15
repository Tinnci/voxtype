#!/usr/bin/env bash
set -euo pipefail

rm -f "$HOME/.local/share/fcitx5/addon/voxtypebridge.conf"
sudo rm -f \
  /usr/lib/fcitx5/libvoxtypebridge.so \
  /usr/share/fcitx5/addon/voxtypebridge.conf

printf 'Fcitx5 addon removed. Restart only Fcitx5 to unload it:\n'
printf '  qdbus6 org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1.Exit\n'
printf '  fcitx5 -d\n'
