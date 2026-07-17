# Fcitx5 integration

VoxType keeps ASR, routing, configuration, and lifecycle in Rust. A small C++
Fcitx5 Module addon is used only for input-context operations because Fcitx5's
stable native addon API is C++.

## What the addon does

- creates `$XDG_RUNTIME_DIR/voxtype/fcitx.sock` with mode `0600`;
- accepts `ARM`, bounded `CONTEXT`, legacy `COMMIT`, idempotent `COMMIT2`,
  `CANCEL`, and `PING` datagrams from the same user;
- records the currently focused Fcitx `InputContext` at `ARM`;
- rejects Password/Sensitive contexts;
- exposes at most 4096 UTF-8 characters of valid surrounding text with adjusted
  character-based cursor/anchor offsets, capability names, truncation state,
  and a generation incremented on focus/capability/text updates;
- rechecks focus, context identity, and secure flags before committing;
- acknowledges dispatch only after the deferred `commitString` call passes its
  final focus/security check. This proves delivery to the Fcitx input-context
  API, not that a target widget rendered or retained the text;
- defers `commitString` to the next Fcitx event-loop turn;
- requires `COMMIT2` to carry a process-unique request ID, caches the most
  recent terminal result, and redirects a duplicate in-flight request to the
  newest reply socket. A timeout retry therefore cannot dispatch the same text
  twice, and a lost ACK can be recovered from the cached result. The completed
  cache keeps only request/session metadata plus a text length/fingerprint, not
  the committed text itself;
- rejects outbound requests above 60 KiB and uses `MSG_TRUNC` to reject
  oversized inbound datagrams instead of parsing truncated text;
- never performs audio capture, network access, secret lookup, or clipboard work.
- exposes one standard Fcitx external-config action that launches
  `voxtype-settings`; all configuration logic remains in the Rust application.

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
~/.local/share/fcitx5/addon/voxtypebridge.conf
```

The user-level copy prevents an older development override from shadowing the
new system metadata. It contains no executable code. The installer does not
replace distribution-owned addons. After installation, restart only Fcitx5
using the commands printed by the script.

On Plasma 6, open **System Settings → Keyboard → Input Method**, show the Fcitx
addons, and configure **VoxType Voice Input Integration**. The page contains a
thin **Open VoxType settings** action. This is intentionally not a full KDE KCM:
it avoids duplicating configuration and keeps the same settings UI usable on
other desktops.

Verify:

```bash
test -S "$XDG_RUNTIME_DIR/voxtype/fcitx.sock"
voxtype doctor
```

Remove only the VoxType addon with `./scripts/uninstall-fcitx-addon.sh`; it also
prints the Fcitx-only restart commands and never requests a system reboot.

For a manual end-to-end check, focus a normal non-password text field and run
the following command from a separately prepared terminal or shortcut:

```bash
voxtype fcitx-insert-test 'VoxType Fcitx 原生提交测试'
```

The command uses the native bridge only. It does not mutate the clipboard or
fall back to synthetic paste. `dispatched=true` proves the final Fcitx call ran;
the text appearing in the focused field is the required frontend delivery
evidence.

`voxtype fcitx-context` prints snapshot metadata without printing application
text. `voxtype grammar context` explicitly reviews selected text, or at most
1200 characters before the cursor, using the local cleanup rules. Password and
Sensitive contexts return an error before any text is copied into Rust.

## Backend selection

`[desktop].insertion_backend` accepts:

- `fcitx`: strict, focus-safe native commit; default;
- `clipboard`: compatibility path using `wl-copy` and `ydotool`;
- `auto`: prefer Fcitx and fall back to copy-only when the bridge is unavailable
  at session start.

Automatic selection never chooses synthetic paste. Once a session has been
armed with Fcitx, a focus or security rejection also never falls back to
clipboard injection. This prevents text from landing in an unexpected
application.

## Verification boundary

The addon is compiled with `-Wall -Wextra -Wpedantic -Werror`, loaded by the
local Fcitx5 5.1.21 process, and its socket/permission/protocol checks pass.
The repository also has Rust unit tests for request-ID ACK matching and bounded
message encoding, while the C++ addon is built with warnings as errors.

An automated Wayland GUI test cannot safely force keyboard focus without user
interaction, so native commit into a real application must be manually checked
in a focused text field. Clipboard delivery also remains a manual desktop check;
an exit status from `ydotool` is not accepted as proof that a widget received
the text.
