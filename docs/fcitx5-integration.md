# Fcitx5 integration

VoxType keeps ASR, routing, configuration, and lifecycle in Rust. A small C++
Fcitx5 Module addon is used only for input-context operations because Fcitx5's
stable native addon API is C++.

## What the addon does

- creates `$XDG_RUNTIME_DIR/voxtype/fcitx.sock` with mode `0600`;
- accepts `ARM`, `COMMIT`, `CANCEL`, and `PING` datagrams from the same user;
- records the currently focused Fcitx `InputContext` at `ARM`;
- rejects Password/Sensitive contexts;
- rechecks focus, context identity, and secure flags before committing;
- defers `commitString` to the next Fcitx event-loop turn;
- never performs audio capture, network access, secret lookup, or clipboard work.

## Build and install

The current system already has the Fcitx5 development package. For a fresh
machine, install the distribution's Fcitx5 development headers and CMake package
first, then run:

```bash
./scripts/install-fcitx-addon.sh
```

The script installs only:

```text
/usr/lib/fcitx5/libvoxtypebridge.so
/usr/share/fcitx5/addon/voxtypebridge.conf
```

It does not replace distribution files. After installation, restart only Fcitx5
using the commands printed by the script.

Verify:

```bash
test -S "$XDG_RUNTIME_DIR/voxtype/fcitx.sock"
voxtype doctor
```

For a manual end-to-end check, focus a normal non-password text field and run
the following command from a separately prepared terminal or shortcut:

```bash
voxtype fcitx-insert-test 'VoxType Fcitx 原生提交测试'
```

The command uses the native bridge only. It does not mutate the clipboard or
fall back to synthetic paste. `queued=true` proves bridge acceptance; the text
appearing in the focused field is the required frontend delivery evidence.

## Backend selection

`[desktop].insertion_backend` accepts:

- `fcitx`: strict, focus-safe native commit; default;
- `clipboard`: compatibility path using `wl-copy` and `ydotool`;
- `auto`: prefer Fcitx and fall back to clipboard only when the bridge is
  unavailable at session start.

Once a session has been armed with Fcitx, a focus or security rejection never
silently falls back to clipboard injection. This prevents text from landing in
an unexpected application.

## Verification boundary

The addon is compiled with `-Wall -Wextra -Wpedantic -Werror`, loaded by the
local Fcitx5 5.1.21 process, and its socket/permission/protocol checks pass.
The repository also has Rust unit tests for bridge response handling.

An automated Wayland GUI test cannot safely force keyboard focus without user
interaction, so native commit into a real application must be manually checked
in a focused text field. The clipboard fallback is separately covered by the
existing `kdialog` integration test.
