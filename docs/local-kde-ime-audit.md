# Local KDE input-method audit

- Audit date: 2026-07-15
- Host: KDE Plasma 6.7.2, KWin Wayland, CachyOS
- Scope: read-only inspection; no reboot or keyboard/input-method changes

## Current input path

```text
physical Japanese 106-key keyboard
        |
        v
libinput -> KWin XKB model jp106 / layout jp
        |
        +--> native Wayland text-input-v3 --> Fcitx5 --> Rime
        |
        +--> XWayland/XIM via XMODIFIERS=@im=fcitx
        |
        +--> sandbox clients via Fcitx/IBus portals
```

Current Fcitx group `默认` contains, in order:

1. `keyboard-jp`
2. `keyboard-us`
3. `rime`, using the `jp` layout

Rime is currently active and its input state is shared across applications. The
graphical XKB model/layout matches a Japanese physical keyboard. The virtual
console keymap remains US; this affects text consoles, not the active Plasma
Wayland session.

## Frontend status

- Fcitx5 controller and Wayland input-method frontend are active.
- KWin virtual keyboard support is enabled.
- Qt 6 has compose, Fcitx5, IBus, and virtual-keyboard input-context plugins.
- `QT_IM_MODULE` and `GTK_IM_MODULE` are intentionally unset, allowing native
  Wayland text-input integration instead of forcing toolkit plugins globally.
- `XMODIFIERS=@im=fcitx` covers XIM/XWayland clients.
- `SDL_IM_MODULE=fcitx` is configured.
- `GLFW_IM_MODULE=ibus` uses the IBus-compatible service exported by Fcitx5.
- Chrome runs natively on Wayland with `--enable-wayland-ime` and text-input v3.
- Fcitx and IBus portal names are both exported for sandbox compatibility.

This is a coherent Plasma Wayland configuration. Setting global
`QT_IM_MODULE=fcitx` or `GTK_IM_MODULE=fcitx` is not recommended without a
specific broken application, because it can bypass the preferred native
Wayland path.

## Voice-input services

### hyprwhspr

- Active user service using local whisper.cpp/Vulkan transcription.
- Observed configuration uses `Super+Alt+D`; a KDE desktop shortcut component
  also has `F9` assigned.
- Reads keyboard devices through evdev and supplies its own OSD and injection
  path.
- Uses substantially more memory while loading/running the local model.

### VoxType

- Active systemd user D-Bus service.
- KDE application shortcut component is registered.
- Intended toggle shortcut: `Meta+Alt+V`; cancel: `Meta+Alt+Escape`.
- Current tested insertion backend is clipboard plus the user-owned ydotoold.
- Current capture backend records through PipeWire-Pulse compatibility.

The shortcuts do not directly conflict, but the two complete dictation stacks
duplicate microphone, shortcut, OSD, lifecycle, and insertion responsibilities.
They should coexist only during migration and comparative testing.

## Recommended target architecture

```text
KDE shortcut / Fcitx action
          |
          v
       voxtyped (Rust)
    capture / routing / ASR
          |
          v
 thin Fcitx5 native bridge
 preedit / commit / surrounding text
          |
          v
 focused application input context
```

The Fcitx bridge should contain no ASR implementation. It talks to the Rust
daemon over the session bus and owns only input-context operations:

- display partial transcript as preedit;
- commit final transcript directly;
- cancel/clear preedit;
- report input purpose, secure-field hints, focus changes, and surrounding-text
  capabilities;
- expose a `VoxType` input-method entry or action in Fcitx configuration.

This avoids normal clipboard mutation, synthetic Ctrl+V, and focus races. The
existing clipboard/ydotool backend remains a fallback for applications that do
not expose a usable Fcitx input context.

## Safe optimizations

1. Keep the current Qt/GTK native Wayland environment policy.
2. Keep the `jp106`/`jp` graphical layout while this is the physical keyboard.
3. Remove `keyboard-us` from the Fcitx group only if it is genuinely unused;
   otherwise it remains a valid explicit English layout.
4. Decide whether Rime activation should be shared across every application or
   remembered per application; this is a user-experience preference, not a
   correctness issue.
5. Consolidate voice services after VoxType has local-provider and OSD parity:
   retain the existing whisper model as a provider, then disable the separate
   hyprwhspr shortcut/daemon.
6. Make Fcitx commit the preferred insertion backend and keep ydotool as an
   explicit compatibility mode.
7. Register one canonical voice shortcut in KGlobalAccel and expose alternate
   language/profile actions as separate shortcuts only when required.

## Additional capabilities enabled by Fcitx integration

- live partial transcript in the composition/preedit area;
- final commit without touching clipboard history;
- focus-safe cancellation when the input context disappears;
- secure/password-field suppression based on input purpose;
- per-application language and provider profiles;
- access to surrounding text for punctuation/spacing normalization;
- voice correction candidates before commit;
- local whisper fallback using the already installed model;
- explicit Chinese, English, and Japanese dictation actions while preserving the
  Japanese physical keyboard layout;
- consistent behavior for Qt, GTK, Chromium/Electron, XWayland, and sandboxed
  clients through Fcitx's existing frontends.
