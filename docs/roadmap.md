# Delivery roadmap

## Phase 0: decisions and spikes

- Resolve licensing and reuse strategy.
- Prototype KDE Global Shortcuts registration.
- Compare unprivileged KWin Wayland insertion backends.
- Capture and resample microphone audio through PipeWire.
- Validate a minimal Rust WebSocket/Opus ASR session with sanitized fixtures.

Exit criterion: select one viable end-to-end KDE path without root privileges.

## Phase 1: headless vertical slice

- Implement daemon state machine and D-Bus CLI.
- Add PipeWire capture and the first ASR provider.
- Implement copy-only output and structured diagnostics.
- Add deterministic provider/audio integration tests.

Exit criterion: `voxtype start` and `voxtype stop` produce a final transcript.

## Phase 2: KDE MVP

- Register push-to-talk and toggle shortcuts.
- Add Plasma tray and notifications.
- Implement the selected insertion backend and clipboard preservation.
- Add KWallet secrets, first-run checks, systemd user service, and packaging.

Exit criterion: daily-use dictation across the P0 application matrix.

## Phase 3: hardening

- Focus-safety policies and secure-field defenses.
- Suspend/resume, device hot-plug, offline, timeout, and retry handling.
- Compatibility reporting, localized UI, and diagnostic bundle.
- Arch and Debian-family packaging plus release automation.

## Phase 4: extensibility

- Local ASR provider.
- Optional transcript normalization profiles and vocabulary.
- GNOME/other compositor adapters.
- Evaluate native Fcitx5 engine integration and Flatpak distribution.

