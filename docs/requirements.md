# VoxType product requirements

## 1. Product definition

VoxType is a Linux desktop voice input service. A user invokes it with a global
shortcut, speaks into a selected microphone, reviews optional live status, and
has the final transcript inserted at the current text cursor.

It is not initially a full language input method, dictation editor, voice
assistant, or Home Assistant service. The first goal is reliable short-form
dictation in normal desktop applications.

## 2. Target users and environments

### P0 target

- Linux desktop users who type primarily in Simplified Chinese.
- KDE Plasma 6 on Wayland.
- PipeWire audio stack.
- Applications using Qt, GTK, Chromium/Electron, and terminal toolkits.
- A single active local desktop session and one microphone.

### P1 target

- KDE Plasma on X11.
- GNOME and other Wayland compositors through portable fallbacks.
- Mixed Chinese/English dictation when supported by a provider.
- Multiple microphones and hot-plugged audio devices.

### Explicitly unsupported in the first release

- Mobile Linux shells and Android containers.
- Multi-seat sessions.
- System/login screen dictation.
- Password fields and secure input surfaces.

## 3. Core user journeys

### R-001 Push-to-talk

The user holds a configurable global shortcut, speaks, and releases it. VoxType
must stop capture, wait for the final result, and insert it into the application
that was focused when recording started.

Acceptance criteria:

- Shortcut works while a normal application has focus on Plasma Wayland.
- Audible or visual feedback appears within 150 ms of activation.
- Releasing the shortcut always ends capture, including after provider errors.
- The transcript is never inserted into a different window silently.

### R-002 Toggle-to-talk

The user presses a shortcut once to start and once to stop. An optional maximum
recording duration protects against accidental indefinite capture.

Acceptance criteria:

- Default maximum duration is configurable and initially 120 seconds.
- A second start request while recording acts as stop, not as a parallel job.
- Escape/cancel discards audio and never inserts partial text.

### R-003 Text insertion

Final text is inserted at the active cursor with predictable clipboard behavior.

Acceptance criteria:

- Preserve and restore clipboard content when the selected backend permits it.
- Never log clipboard content or transcripts at normal log levels.
- Show a clear error if the compositor blocks synthetic input.
- Offer copy-only mode as a safe fallback.
- Do not inject into recognized password or secure fields when detectable.

### R-004 Status feedback

The user can distinguish idle, connecting, listening, speech detected,
transcribing, inserting, complete, cancelled, and error states.

Acceptance criteria:

- Plasma tray status is sufficient for the MVP; a floating overlay is P1.
- Errors include a short user message and a diagnostic code.
- Intermediate ASR text is display-only by default and is never committed as
  final input.

### R-005 First-run setup

The first-run flow validates microphone access, provider credentials, shortcut
registration, and insertion support.

Acceptance criteria:

- A diagnostic command can test each subsystem independently.
- Secrets are stored through Secret Service/KWallet where available.
- Configuration files contain secret references, not raw tokens, by default.

## 4. KDE and desktop integration

### R-100 Global shortcut

- P0: register actions with KDE's Global Shortcuts infrastructure over D-Bus or
  provide generated Plasma shortcut entries when registration is unavailable.
- Expose separate start, stop, toggle, and cancel commands, with independent
  Plasma actions so users can bind push-to-talk helpers without replacing the
  toggle shortcut.
- Avoid requiring root privileges or raw `/dev/input` access for shortcuts.
- Detect registration conflicts and explain how to resolve them.

### R-101 Wayland text input

Text insertion must use a capability-negotiated backend, in this preferred order:

1. Native input-method integration when a stable KDE/Fcitx5 path is available.
2. Portal or compositor-supported virtual keyboard mechanism.
3. Clipboard plus a user-authorized paste action.
4. Copy-only fallback.

The implementation must not claim universal Wayland support when only an
external `uinput` helper is available.

### R-102 Focus safety

- Record the target surface identity at activation when the desktop API allows.
- Before insertion, verify that focus is unchanged or request confirmation.
- Allow a configurable policy: strict, prompt, or current-focus.
- Default to strict on the first stable release.

### R-103 Tray and notifications

- Implement StatusNotifierItem compatibility for Plasma.
- Provide start/stop, cancel, settings, diagnostics, and quit actions.
- Use freedesktop notifications for recoverable errors.
- Do not put transcript content in notifications by default.

### R-104 Autostart and session lifecycle

- Ship a systemd user service and XDG autostart fallback.
- Wait for the graphical session, D-Bus, and PipeWire availability.
- Recover after PipeWire and network restarts without requiring logout.
- Ensure only one daemon instance runs per user session.

## 5. Audio requirements

### R-200 Capture

- Use PipeWire through a maintained Rust abstraction or direct integration.
- Default to the system-selected input; permit persistent device selection.
- Accept common device formats and resample internally to provider needs.
- Handle device removal, suspension, and sample-rate changes.

### R-201 Processing

- First provider contract: mono, 16 kHz PCM internally, encoded to 20 ms Opus
  frames when required.
- Use bounded channels so a slow network cannot grow memory without limit.
- Report clipping and unusually low input levels in diagnostics.
- Noise suppression, AGC, and echo cancellation are opt-in future features.

### R-202 Voice activity and endpointing

- Prefer provider VAD for initial delivery compatibility.
- Add optional local VAD later to reduce uploaded silence and improve stop UX.
- Never end push-to-talk solely because local VAD detects silence.

## 6. Speech provider requirements

### R-300 Provider abstraction

Each provider must implement session creation, streaming audio, intermediate
events, final result, cancellation, timeout, credential refresh, and structured
error mapping.

### R-301 Doubao provider

- Reuse behavioral knowledge from `doubao-asr-for-ha` only under a documented
  licensing decision.
- Preserve 16 kHz mono Opus framing, bounded backpressure, VAD/interim/final
  event handling, token redaction, timeout handling, and one credential refresh
  retry where applicable.
- Clearly label the provider unofficial unless an official API is used.
- Never present reverse-engineered device registration as an official service.

### R-302 Future providers

- Permit official cloud ASR APIs and local engines such as whisper.cpp without
  changes to desktop integration.
- Provider selection is per profile; automatic cloud failover is opt-in.
- Do not upload audio to more than one provider without explicit consent.

### R-303 Transcript normalization

- Preserve provider output by default.
- Optional transformations include whitespace cleanup, punctuation preferences,
  Chinese/English spacing, numeral styles, and custom vocabulary replacements.
- Transformations must be deterministic, previewable, and independently
  disableable.

## 7. Configuration and CLI

### R-400 Commands

Planned command surface:

```text
voxtype daemon
voxtype start
voxtype stop
voxtype toggle
voxtype cancel
voxtype status [--json]
voxtype doctor
voxtype devices
voxtype config path
```

### R-401 Configuration

- Follow XDG base directories.
- Use TOML for non-secret configuration.
- Support profiles for provider, language, microphone, insertion policy, and
  post-processing.
- Validate configuration at startup and preserve unknown future fields when a
  settings UI edits the file.
- Environment variable secret overrides are allowed for CI/testing only.

### R-402 IPC

- The CLI controls the daemon over the user session D-Bus.
- The API exposes state changes, partial transcript events, final results, and
  diagnostic summaries.
- D-Bus methods must reject callers outside the current user session.
- The stable public interface uses only D-Bus scalar values, strings, arrays,
  and string-keyed dictionaries so clients do not depend on Rust serialization.
- Every state-changing request returns a session ID or a structured error; it
  must not report success before the state machine accepts the transition.
- D-Bus and CLI compatibility follows semantic versioning independently from
  the internal Rust module layout.

### R-403 API compatibility

- Public D-Bus interface: `io.github.tinnci.VoxType1`.
- Public object path: `/io/github/tinnci/VoxType1`.
- Provider implementations are private plugins at first; no stable Rust plugin
  ABI is promised in version 0.x.
- Internal traits must not expose Tokio, zbus, cpal, tungstenite, PipeWire, Qt,
  or provider-specific types.
- API payloads carry opaque IDs rather than filesystem paths or secret values.
- All externally visible enums include an `unknown`/forward-compatible mapping.

## 8. Privacy and security

### R-500 Data handling

- Display a clear disclosure that cloud providers receive microphone audio.
- No recording or transcript persistence by default.
- Diagnostic bundles redact tokens, device identifiers, transcript text, and
  clipboard contents.
- Optional history, if added, is off by default and has one-click deletion.

### R-501 Secret handling

- Prefer KWallet through Secret Service-compatible APIs.
- Keep secrets out of command-line arguments, process listings, logs, crash
  reports, and shell history.
- Restrict fallback credential files to user-only permissions.

### R-502 Process privileges

- Main daemon runs unprivileged.
- Avoid `uinput` and root helpers in the default KDE path.
- Any optional privileged helper must be separately packaged, narrowly scoped,
  authenticated, and disabled by default.

## 9. Reliability and performance

### R-600 State machine

The daemon has one explicit session state machine. Invalid transitions return
errors rather than spawning overlapping capture/provider tasks.

### R-601 Latency targets

- Shortcut-to-feedback: p95 below 150 ms.
- Shortcut-to-audio-capture: p95 below 250 ms.
- End-of-speech to final insertion: p95 below 2 seconds on a healthy network,
  tracked separately from provider latency.
- No unbounded audio, event, or log queues.

### R-602 Failure recovery

- Cancellation propagates through capture, encoder, transport, and UI layers.
- Network and authentication failures leave the daemon ready for the next try.
- A provider request timeout never leaves the microphone active.
- Crash recovery must not insert a stale transcript after restart.

### R-603 Observability

- Structured local logs with request IDs and phases.
- Metrics include capture duration, audio frames, dropped frames, first partial
  latency, final latency, insertion backend, and failure phase.
- Transcript content and tokens are excluded unless a one-shot, explicit debug
  consent is enabled.

## 10. Accessibility and localization

### R-700 Accessibility

- All tray/settings actions are keyboard reachable.
- Do not rely on color alone for recording state.
- Support optional sounds and optional visual feedback independently.
- Respect reduced-motion preferences for future overlays.

### R-701 Localization

- Simplified Chinese and English UI/documentation are P0.
- Keep user-facing strings externalizable from the first UI implementation.
- Provider language capability must be reported accurately; bilingual UI does
  not imply bilingual recognition.

## 11. Packaging and compatibility

### R-800 Packages

- Produce a standalone release archive first.
- Plan packages for Arch Linux and Debian/Ubuntu; Flatpak feasibility requires
  a separate investigation because global shortcuts and text injection cross
  sandbox boundaries.
- Install desktop, D-Bus, systemd user, icon, and autostart metadata in standard
  locations.

### R-801 Compatibility matrix

Every release records results for Plasma/Wayland with at least one Qt app, GTK
app, Chromium/Electron app, LibreOffice, and terminal, plus the available X11
fallback. Unsupported combinations must be documented rather than hidden.

## 12. Testing requirements

### R-900 Unit and protocol tests

- Audio frame splitting and resampling boundaries.
- Provider request encoding and response parsing using sanitized fixtures.
- State-machine transitions, cancellation, timeouts, and secret redaction.
- Transcript transformation and configuration validation.

### R-901 Integration tests

- Fake PipeWire/audio source to deterministic provider mock.
- D-Bus CLI-to-daemon control.
- Clipboard preservation and insertion backend selection.
- Provider backpressure and reconnect behavior.

### R-902 Desktop acceptance tests

- Manual release checklist on a real Plasma Wayland session.
- Verify target applications, focus change safety, shortcut conflicts, tray
  lifecycle, suspend/resume, microphone hot-plug, offline mode, and KWallet
  locked/unlocked states.

## 13. Non-functional project requirements

### R-950 Code quality

- Stable Rust with rustfmt, Clippy warnings denied in CI, and no unsafe code in
  first-party crates unless justified by an ADR and isolated.
- Separate crates/modules for domain state, audio, providers, desktop adapters,
  storage, and CLI once complexity requires a workspace.
- Provider and desktop integrations are tested behind traits.
- Prefer ordinary functions, enums, and object-safe traits over proc-macro-heavy
  frameworks in core code.
- Core domain and protocol-codec modules must compile without desktop, audio,
  networking, or async-runtime dependencies.

### R-953 Dependency budget

- Standard library first; every direct dependency requires a documented owner,
  purpose, feature list, and removal strategy.
- Default builds include only the daemon, CLI, KDE D-Bus integration, selected
  audio backend, and one explicitly enabled provider.
- Disable dependency default features unless they are audited and required.
- Avoid duplicate TLS, HTTP, JSON, D-Bus, logging, and async runtime stacks.
- Prefer small handwritten codecs for genuinely tiny stable wire formats, with
  fuzz/property tests; do not handwrite security-sensitive codecs.
- `cargo tree -d`, binary size, clean build time, and incremental build time are
  release metrics.
- A new proc-macro dependency or native build script requires justification in
  review because both can materially increase clean builds.

### R-951 Supply chain

- Commit `Cargo.lock` for the application.
- Automated dependency audit and license review.
- Reproducible release inputs and checksums; signing is a P1 objective.

### R-952 Legal gates

Before implementing the Doubao provider:

1. Confirm ownership and contributor rights in the reference repository.
2. Decide whether VoxType uses PolyForm Noncommercial, receives a relicense, or
   uses a clean-room implementation based only on independently documented
   behavior.
3. Preserve required notices for any reused material.
4. Document provider terms, unofficial status, privacy disclosure, and allowed
   distribution model.

## 14. MVP release gate

Version 0.1 is ready only when all of the following are true:

- Push-to-talk works on Plasma 6 Wayland without root.
- One provider completes real Chinese dictation.
- Final text is inserted into the agreed compatibility matrix or safely copied
  with an explicit warning.
- Focus-change and secure-field protections have been exercised.
- Microphone, network, provider, and insertion failures recover cleanly.
- Secrets and transcripts are absent from normal logs and diagnostics.
- Setup, privacy, licensing, and known limitations are documented.
