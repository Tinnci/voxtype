# Architecture outline

## Component boundaries

```text
KDE shortcut / CLI
        |
        v
  D-Bus control API -----> tray / notifications
        |
        v
 session state machine
   |        |         |
   v        v         v
audio -> provider -> transcript pipeline
                       |
                       v
             text insertion backend
```

The session state machine owns cancellation and is the only component allowed
to move a dictation request from idle through capture, recognition, insertion,
and completion.

## Planned Rust modules

- `domain`: session states, events, errors, policies, and provider-neutral types.
- `audio`: PipeWire capture, format conversion, level diagnostics, and framing.
- `provider`: provider trait plus Doubao and future local/cloud implementations.
- `desktop-kde`: global shortcut, tray, notification, focus, and insertion
  adapters for Plasma.
- `ipc`: D-Bus daemon API and CLI client.
- `config`: XDG configuration, profiles, migrations, and secret references.
- `app`: orchestration and lifecycle.

## Important design rules

- Desktop and provider code depend on domain interfaces, not on each other.
- Audio queues are bounded and cancellation-aware.
- Intermediate transcripts are events; only final transcripts may be inserted.
- Text insertion is a negotiated capability with a copy-only fallback.
- Secrets are represented by opaque references outside the secret-store module.
- Provider-specific identifiers never become the application's core identity.

## Open investigations

1. Plasma 6 global shortcut D-Bus registration and packaging requirements.
2. Best unprivileged text insertion path on KWin Wayland: native input method,
   Fcitx5 integration, portal/virtual keyboard, or clipboard-mediated action.
3. Reliable focused-surface identity and secure-field detection on Wayland.
4. PipeWire integration crate choice and real-time callback constraints.
5. KWallet/Secret Service interoperability in a pure Rust process.
6. Flatpak feasibility for global shortcuts, microphone, and insertion.

