# VoxType

VoxType is a planned KDE-first voice typing service for Linux. Press a global
shortcut, speak, and insert the recognized text into the currently focused
application.

> Status: active development. D-Bus control, PipeWire-Pulse capture, KDE shortcuts,
> Secret Service configuration, a deterministic test provider, and an
> OpenAI-compatible REST provider are implemented. Fcitx-native commit and the
> Doubao adapter remain in progress.

## Why VoxType

The name describes the user-facing capability rather than a specific cloud
provider or Home Assistant integration. The architecture will keep speech
providers, desktop integration, audio capture, and text insertion replaceable.

## Initial product direction

- Rust implementation, with a small long-running desktop daemon and CLI.
- KDE Plasma 6 and Wayland as the primary environment.
- KDE Global Shortcuts for push-to-talk and toggle-to-talk actions.
- Safe text insertion with explicit, observable fallback behavior.
- Doubao ASR as the first provider, based on lessons from
  [`doubao-asr-for-ha`](https://github.com/Tinnci/doubao-asr-for-ha).
- Provider-neutral interfaces so local and official cloud ASR backends can be
  added later.

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
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run --bin voxtyped
```

## Licensing status

No source code has been ported from the reference project. Its PolyForm
Noncommercial 1.0.0 license must be respected. The license for VoxType and the
method used to reuse or independently implement provider protocol details must
be decided before ASR provider code is added.
