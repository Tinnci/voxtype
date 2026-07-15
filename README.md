# VoxType

VoxType is a KDE-first voice typing service for Linux. Press a global
shortcut, speak, and insert the recognized text into the currently focused
application.

> Status: active development. The local Plasma/Wayland vertical slice is
> operational; native Fcitx delivery still requires final manual application
> matrix verification, and production provider profiles are not configured by
> default.

## Why VoxType

The name describes the user-facing capability rather than a specific cloud
provider or Home Assistant integration. The architecture will keep speech
providers, desktop integration, audio capture, and text insertion replaceable.

## Implemented components

- Rust daemon and CLI over a per-user D-Bus API.
- PipeWire-Pulse microphone capture at mono 16 kHz signed 16-bit PCM.
- Plasma KGlobalAccel shortcuts, StatusNotifierItem tray, D-Bus menu, and
  freedesktop notifications.
- Focus-locked Fcitx5 native commit with secure-field rejection, plus an
  explicit clipboard/ydotool compatibility backend.
- OpenAI-compatible REST, deterministic mock, and isolated local-command
  providers with fallback health tracking.
- XDG TOML configuration and KWallet/Secret Service credential references.
- Hardened systemd user services and user-level desktop/D-Bus packaging.

## Install on Plasma 6

Build and install user-owned components:

```bash
./scripts/install-user.sh
```

The native Fcitx bridge is a small C++ addon and must be installed separately
because Fcitx loads addons from system directories:

```bash
./scripts/install-fcitx-addon.sh
```

The addon installer prints the commands needed to restart only Fcitx5. A system
reboot is not required.

Verify the complete local stack:

```bash
voxtype doctor
voxtype doctor audio
voxtype fcitx-focus
voxtype providers
```

The default profile is a local deterministic mock and does not upload audio.
Configure a real provider in `~/.config/voxtype/config.toml`; see
[Configuration and providers](docs/configuration.md).

Default Plasma shortcuts:

- `Meta+Alt+V`: start or stop dictation.
- `Meta+Alt+Escape`: cancel dictation.

Use the microphone tray item for the same actions and provider health status.

## Documents

- [Product requirements](docs/requirements.md)
- [Architecture](docs/architecture.md)
- [API contracts](docs/api-contracts.md)
- [Doubao protocol analysis](docs/doubao-api-analysis.md)
- [Dependency and build policy](docs/dependencies-and-builds.md)
- [Local KDE input-method audit](docs/local-kde-ime-audit.md)
- [Configuration and providers](docs/configuration.md)
- [Fcitx5 integration](docs/fcitx5-integration.md)
- [Delivery roadmap](docs/roadmap.md)
- [ADR 0001: Rust](docs/decisions/0001-rust.md)
- [ADR 0002: system boundaries and Cargo workspace](docs/decisions/0002-system-boundaries-and-workspace.md)

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run --bin voxtyped
```

Run the same release installation path used for local integration testing:

```bash
./scripts/install-user.sh
```

## Licensing status

No source code has been ported from the reference project. Its PolyForm
Noncommercial 1.0.0 license must be respected. The license for VoxType and the
method used to reuse or independently implement provider protocol details must
be decided before ASR provider code is added.
