#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
build_dir="$repo_root/build/fcitx5-addon"

cd "$repo_root"
cmake -S fcitx5-addon -B "$build_dir" -G Ninja \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DCMAKE_INSTALL_PREFIX=/usr
cmake --build "$build_dir"
install -Dm644 fcitx5-addon/voxtypebridge.conf \
  "$HOME/.local/share/fcitx5/addon/voxtypebridge.conf"
sudo cmake --install "$build_dir" --prefix /usr

printf 'Fcitx5 addon installed. Restart only Fcitx5 to load it:\n'
printf '  qdbus6 org.fcitx.Fcitx5 /controller org.fcitx.Fcitx.Controller1.Exit\n'
printf '  fcitx5 -d\n'
