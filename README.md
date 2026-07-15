# VoxType

VoxType is a KDE-first voice typing service for Linux. Press a global
shortcut, speak, and insert the recognized text into the currently focused
application.

> Status: active development. The Plasma 6/Wayland vertical slice is
> operational on the development machine. Production provider profiles are not
> configured by default, and native Fcitx delivery still requires manual checks
> in each target application family.

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
- Qt 6 settings panel for provider status, safe API-key updates, VAD/input
  settings, session-local consumption, and user-defined soft quotas.
- Frameless KDE overlay, local energy VAD, and an in-memory recent-transcript
  grammar/typography checker.
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
voxtype usage
```

The default profile is a local deterministic mock and does not upload audio.
Configure a real provider in `~/.config/voxtype/config.toml`; see
[Configuration and providers](docs/configuration.md).

Default Plasma shortcuts:

- `Meta+Alt+V`: start or stop dictation.
- `Meta+Alt+Escape`: cancel dictation.
- `Meta+Alt+G`: check the most recently inserted transcript locally.

Open `VoxType Settings` from the application launcher, run
`voxtype-settings`, or use the microphone tray menu. The Fcitx5 Input Method
KCM also exposes VoxType through the configurable bridge addon.

## Documents

- [Product requirements](docs/requirements.md)
- [Architecture](docs/architecture.md)
- [API contracts](docs/api-contracts.md)
- [Doubao protocol analysis](docs/doubao-api-analysis.md)
- [Dependency and build policy](docs/dependencies-and-builds.md)
- [Local KDE input-method audit](docs/local-kde-ime-audit.md)
- [Configuration and providers](docs/configuration.md)
- [Fcitx5 integration](docs/fcitx5-integration.md)
- [Platform support](docs/platform-support.md)
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

## Reference-code policy

No source code has been ported from the original Home Assistant project or from
Rime. Their public designs are used only to inform system boundaries and API
analysis; VoxType implementations and tests are written independently.
