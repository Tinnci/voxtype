# Architecture outline

## Component boundaries

```text
KDE shortcut / CLI / settings QML
        |
        v
  D-Bus control API -----> tray / notifications / overlay
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

After capture stops, provider work runs in a named background thread with a
bounded one-result channel and a shared cancellation token. The D-Bus object
remains available for status/cancel calls. The main owner applies a result only
when its session ID still matches the current `finalizing` state; cancellation
terminates curl or the complete command-provider process group and removes the
recording path.

## System boundaries

VoxType has four primary boundaries:

1. **Core policy boundary**: session state, routing decisions, provider-neutral
   audio/transcript types, error taxonomy, and privacy policy.
2. **Desktop boundary**: KDE shortcuts, focus tracking, tray, notifications,
   secret store, and text insertion.
3. **Capture boundary**: microphone discovery, capture, conversion, and bounded
   audio delivery.
4. **Provider boundary**: credentials, network protocol, provider framing,
   result parsing, and provider-specific diagnostics.

The core chooses a provider and owns fallback policy. Providers do not call each
other, access the desktop, insert text, or decide whether audio may be sent to a
second service.

## Current packages and adapters

- `voxtype-core`: dependency-free session state, routing policy, provider-neutral
  errors, and audio contracts.
- `voxtype-provider-rest`: WAV staging, system-curl transport, JSON response and
  provider-reported usage parsing.
- `voxtype-provider-deepgram`: official Deepgram binary-upload request and
  nested transcript response parsing.
- `voxtype-provider-common`: audited credential redaction, endpoint validation,
  and PCM-to-WAV staging shared only after two cloud adapters proved the need.
- root `voxtype` package: daemon orchestration, audio process adapter, D-Bus,
  configuration, Secret Service, VAD, grammar, Fcitx/clipboard insertion, and
  thin CLI/tray/settings/overlay binaries.
- `fcitx5-addon`: small C++ focus-safe input-context bridge and external settings
  launcher; it contains no ASR or configuration policy.

Further crates are justified only by a materially different protocol, SDK,
native dependency, or licensing/failure boundary. A likely future shape is:

```text
voxtype/
  crates/
    voxtype-core/             # dependency-light policy and contracts
    voxtype-app/              # orchestration if the root crate becomes too broad
    voxtype-provider-doubao/  # unofficial protocol, isolated license/risk
    voxtype-provider-<name>/  # another online provider
  src/bin/
    voxtype.rs                # thin CLI
    voxtyped.rs               # daemon entry point
```

This is not a requirement to create every package. `voxtype-core` and providers
with different risk/dependency profiles are the valuable boundaries. Tiny
facade crates or one-trait packages remain explicitly discouraged.

## Important design rules

- Desktop and provider code depend on domain interfaces, not on each other.
- Audio queues are bounded and cancellation-aware.
- Intermediate transcripts are events; only final transcripts may be inserted.
- Text insertion is a negotiated capability with a copy-only fallback.
- Automatic insertion fallback is allowed only for an unavailable Fcitx
  transport; focus and secure-field rejection are terminal safety decisions.
- Secrets are represented by opaque references outside the secret-store module.
- Provider-specific identifiers never become the application's core identity.
- Core interfaces use `std` types and project-owned newtypes; adapters translate
  third-party crate types at the boundary.
- The MVP favors a small thread-based runtime with bounded standard-library
  channels. An async runtime is an adapter-level decision, not a domain rule.
- No dynamic Rust plugin ABI is promised. Providers are compiled features until
  process-isolated plugins have a demonstrated need.
- One provider handles a session by default. Parallel provider racing is off by
  default because it duplicates audio disclosure and cost.
- Fallback is decided before upload when possible. Replaying buffered speech to
  a second provider requires explicit profile consent.
- Every provider passes the same contract-test suite; multiple implementations
  are evidence that the abstraction is real, but mocks still cover edge cases.

## Open investigations

1. Plasma 6 global shortcut D-Bus registration and packaging requirements.
2. Best unprivileged text insertion path on KWin Wayland: native input method,
   Fcitx5 integration, portal/virtual keyboard, or clipboard-mediated action.
3. Reliable focused-surface identity and secure-field detection on Wayland.
4. PipeWire integration crate choice and real-time callback constraints.
5. KWallet/Secret Service interoperability in a pure Rust process.
6. Flatpak feasibility for global shortcuts, microphone, and insertion.
7. Benchmark blocking threads versus an async runtime for the single-session
   desktop workload before adding Tokio to the default dependency graph.
