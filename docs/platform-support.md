# Platform support

VoxType targets current Linux desktop stacks rather than old distribution
releases. The Rust daemon and provider code are independent of KDE, while
first-class integration is deliberately Plasma 6/Wayland/Fcitx5.

## Supported target

| Area | Target |
| --- | --- |
| Desktop session | KDE Plasma 6 on Wayland |
| Input method | Fcitx5 5.1.7 or newer; Rime remains an ordinary Fcitx engine |
| Audio | PipeWire with PulseAudio compatibility and `parec` |
| Settings/overlay | Qt 6 QML runtime and Qt Quick Controls |
| Service manager | systemd user services |
| Secret storage | Secret Service via `secret-tool`; KWallet is the KDE backend |
| Rust | MSRV 1.85, edition 2024 |

The development machine runs CachyOS/Arch rolling, Plasma 6.7.2, Fcitx5
5.1.21, Qt 6.11, and PipeWire 1.6. CI also builds the Rust workspace and Fcitx
addon on Ubuntu 24.04 to catch older compiler/header assumptions, but Ubuntu
24.04's default Plasma desktop is not claimed as a full GUI test target.

## Distribution compatibility

Source builds are intended to work on modern Arch-derived, Fedora KDE, openSUSE
Tumbleweed, and current Plasma 6 Debian/Ubuntu-derived systems when their
equivalent runtime/development packages are installed. Package names differ, so
the repository currently ships installation scripts rather than pretending to
be a universal distro package.

Prebuilt glibc binaries must be built on the oldest glibc baseline a release
wants to support. Building from source avoids that binary-ABI issue. VoxType has
no kernel-module dependency and does not require a system reboot.

## Compatibility layers

- The Fcitx bridge is the preferred safe insertion path and rejects password or
  sensitive contexts.
- Clipboard plus `ydotool` is an explicit fallback, not equivalent security.
  The daemon service does not start or require `ydotool`; users who explicitly
  select the clipboard-paste backend manage that optional helper separately.
- The settings application is standalone Qt/QML. KDE adds tray, shortcut,
  notification, desktop-entry, and Fcitx-KCM entry points without owning the
  configuration engine.
- Rime is neither forked nor modified. It remains a peer Fcitx input engine;
  VoxType commits final text through the focused Fcitx input context.

## Not currently supported as first class

- Plasma 5 and legacy Qt 5 desktops;
- non-systemd service installation;
- X11-specific focus and synthetic-input behavior;
- Flatpak/Snap confinement;
- GNOME integration beyond the standalone daemon/CLI and explicit clipboard
  fallback;
- musl prebuilt binaries.

These are product-scope choices, not necessarily permanent technical limits.
Keeping the Rust core, QML frontend, and Fcitx adapter separate allows another
desktop adapter without changing provider or session policy.
