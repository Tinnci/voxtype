# Implementation layers and replacement plan

This document separates production behavior, deliberate test doubles, and
contracts that are not yet connected to the running daemon. A green mock flow
is never evidence of ASR quality or desktop delivery correctness.

## Layer 0: session policy and orchestration

Owns session IDs, state transitions, cancellation, provider routing, replay
consent, usage semantics, and stale-result rejection. Provider execution is now
off the D-Bus object lock with session-checked result delivery and cancellable
curl/command children. Provider failures carry
transport-started plus conservative `NotAccepted`, `PossiblyAccepted`, or
`Accepted` evidence. Fallback and usage accounting no longer infer audio
exposure from the provider type; ambiguous cancellation or connection loss
requires explicit buffered replay consent.

The remaining boundary issue is that the running daemon still dispatches a
configuration enum instead of the reusable core provider trait/registry. The
next orchestration increment is a single `ProviderAdapter` contract returning
an attempt outcome with lifecycle evidence, followed by a bounded event queue
that emits every state transition rather than reconstructing transitions from
a 100 ms snapshot.

## Layer 1: capture and speech signal processing

Owns devices, negotiated formats, resampling, bounded audio chunks, level and
clipping metrics, VAD, endpointing, and the recording spool. `pw-record` is the
preferred native PipeWire capture adapter; `parec` remains a compatibility
backend only when the native command is unavailable. Both expose stream
failures directly while keeping the provider-facing format at mono 16 kHz PCM.

The capture reader now publishes a bounded stream of 20 ms RMS, peak, and
clipping metrics without retaining audio in the UI path. The daemon consumes
those frames with the same stateful VAD used for live speech indication and
updates the overlay with structured level fields. Dropped telemetry never
blocks PCM recording; the final recognition path still performs authoritative
offline trim and provider upload checks.

The current energy VAD is real deterministic code but intentionally small. It
must evolve into a streaming stateful detector with a DC blocker, short/long
energy, noise updates only outside speech, SNR hysteresis, hangover, and bounded
pre-roll. Push-to-talk never ends solely on VAD; toggle mode may opt into local
endpointing.

## Layer 2: provider protocols and transport

OpenAI-compatible REST and Deepgram are real batch protocols. Their loopback
servers and synthetic PCM are correct test doubles and remain in CI. The shared
curl result contains exit code, HTTP status, bounded body, and upload evidence;
redirects cannot forward secrets to another origin. Provider IDs, routing,
cancellation, and lifecycle/usage types belong to the core/application domain
rather than one provider crate.

The deterministic mock provider remains available only as an explicit demo and
test fixture. It must not be the completed first-run experience or count as a
cloud request/quota consumption event. The settings panel now labels the demo
and can create a real provider/profile; a fuller onboarding flow still needs a
credential test and live recognition confirmation.

## Layer 3: KDE, Fcitx, and insertion

KGlobalAccel registration, StatusNotifierItem registration, the Fcitx input
context, secure-field flags, focus watch, D-Bus, and systemd services are real
system integrations. Required work is event correctness:

- a real press/release shortcut adapter for push-to-talk;
- background recognition so cancel/status remain responsive;
- Fcitx success only after the final deferred commit (implemented);
- no automatic clipboard downgrade after focus/security rejection (implemented);
- state signals that drive a live tray and persistent overlay.

`copy` is the portable no-injection fallback and the only automatic fallback
when Fcitx is unavailable. Clipboard plus `ydotool` remains an explicit unsafe
compatibility choice and is not a daemon service dependency.
The daemon emits ordered `StateChanged(state, session)` lifecycle events from a
bounded queue populated immediately after each state-machine transition. Short
`finalizing` and `inserting` states are no longer reconstructed from 100 ms
snapshots. A transcript-free `SessionFinished` event reports the final outcome,
stable error code, insertion backend, and character count for CLI/automation.
The tray uses state events for normal SNI icon/status and dbusmenu updates,
while a five-second status read remains only as disconnect/reconnect protection.

## Layer 4: user experience, configuration, and diagnostics

Owns provider/profile onboarding, microphone selection, calibration, quota
labels, privacy disclosure, history consent, diagnostics, and accessibility.
The current grammar module is a real local typography normalizer, not a full
grammar model. It should be named and presented accurately; a future grammar
backend may use LanguageTool or another explicitly configured local/online
service without putting that dependency in the core daemon.

Audio calibration now separates quiet and speaking phases and reports noise
distribution, speech distribution, SNR, clipped-sample ratio, speech ratio,
confidence, and a suggested threshold. Low-confidence results cannot be
applied. `doctor audio` remains a shorter capture-path diagnostic. Device
identity, startup latency, and captured/dropped frame counters remain the next
capture-adapter increment.
Session-local usage and soft quotas are not provider billing balances and must
remain labelled as such.

## Layer 5: verification and delivery

Keep fake capture, loopback HTTP, synthetic PCM, and command-process fixtures.
Add D-Bus daemon integration, provider contract tests, bounded backpressure,
focus-change/secure-field checks, suspend/hot-plug recovery, and an opt-in live
ASR smoke test. The release matrix covers Qt, GTK, Chromium/Electron,
LibreOffice, and a terminal on Plasma Wayland.

## Delegated work packages

1. Core orchestration: cancellable provider jobs, lifecycle-aware routing,
   session generation guards, and truthful usage/health.
2. Capture/DSP: PipeWire device adapter, bounded chunks, resampling, streaming
   VAD/endpointing, and audio metrics.
3. Provider transport: HTTP status/exit classification, redirect policy,
   bounded output, and shared batch contracts.
4. KDE event path: push-to-talk, D-Bus state signals, responsive tray/overlay,
   and final commit acknowledgement.
5. Onboarding/verification: provider and profile CRUD, microphone UI,
   diagnostics, fake-capture integration, and opt-in live smoke tests.

Work packages 1 and 2 define the interfaces and can proceed in parallel. Work
packages 3 and 4 build against those lifecycle events. Package 5 continuously
adds acceptance evidence rather than waiting until the end.

The current audit delegated ownership as follows:

- Provider/runtime: attempt lifecycle, cancellation, fallback, usage, quota,
  provider registry, and production isolation of the deterministic provider.
- Capture/DSP: device selection, online PCM frames, VAD, endpointing,
  calibration, clipping/SNR metrics, and hot-plug behavior.
- Desktop/UX: ordered D-Bus events, final session outcome, Fcitx context and
  idempotent commit, push-to-talk, overlay telemetry, and reviewable cleanup UI.

## Mock replacement inventory

Replace a double only when it can affect a normal user path. Keep deterministic
doubles at protocol and process boundaries so failure behavior remains testable.

### P0: remove from normal product paths

- A fixed-text provider must never be mistaken for successful ASR. A default
  demo-only profile is now rejected by the normal no-profile start action;
  developers may still name that profile explicitly for insertion integration
  tests. First-run UI must lead to a real cloud or local provider.
- Focused-text cleanup now reads a bounded Fcitx snapshot only after the
  explicit shortcut and never stores it. VoxType transcript history remains a
  separate opt-in memory feature for `grammar last/history`.
- Any provider `success` must come from a parsed non-empty provider response or
  a real local command result, never a generated sample string.
- Provider readiness no longer calls a configured secret or an untripped
  circuit breaker "available". `ProviderStatus` is versioned JSON, real
  successes create a 15-minute verified state, recent failures remain visible,
  and configured providers without evidence remain `configured, unverified`.
  The settings summary counts only verified providers as usable.
- D-Bus must expose an ordered final session outcome. `Stop` returning
  `result=processing` is only acceptance, not recognition success; clients need
  a single completion/failure/cancel event and stable error code without
  requiring transcript history.

### P1: replace approximations with truthful production algorithms

- Evolve audio capture from the `parec` compatibility process to a PipeWire
  stream with negotiated device identity, bounded buffers, drop accounting,
  resampling, and hot-plug recovery.
- Keep the stateful energy VAD, but add DC rejection, independently smoothed
  short/long energy, speech-band evidence, calibrated SNR hysteresis, and live
  endpoint events. Feed the recording UI and final trim from the same streaming
  detector. This is an algorithm improvement, not replacement of a mock.
- Present `grammar.rs` as local typography/text cleanup. It now returns
  versioned JSON with Unicode-safe byte spans, exact original/replacement text,
  stable rule IDs, confidence, safety, and a whole-source fingerprint. Newlines,
  ellipses and URL tokens are preserved; repeated-word, repeated-punctuation,
  whitespace-collapse and capitalization suggestions require review. A feature
  called full semantic grammar checking still needs a separate pluggable local
  or online checker. `voxtype-cleanup` now renders a private-file-backed Qt 6
  review window; apply/undo remains disabled until the Fcitx context generation
  can be atomically checked during replacement.
- The overlay is now a persistent QML process backed by a 0600 runtime state
  file. State transitions use bounded stdin JSON, while high-rate audio
  telemetry uses the same directory's 0600 atomic state replacement so the
  daemon does not spawn a process for every frame. State text is no longer
  placed in a process argv. The listening view now renders RMS/threshold/
  speech-active telemetry and keeps processing progress explicitly
  indeterminate. Ordered provider stages are still needed for a truthful
  upload/fallback progress indicator. Live telemetry preserves the required
  `visible=true` field, so an audio-level update cannot hide the listening
  window by replacing the state snapshot with a partial payload.
- Treat request/audio/token counters as daemon-session telemetry and configured
  limits as local soft limits. Provider account balance or billing quota may be
  shown only when fetched from an authoritative provider API with provenance.
- Calibration now uses guided silence and speech phases with noise/speech
  distributions, SNR, clipped-sample ratio, threshold confidence, and guarded
  application. Device identity and hot-plug evidence remain capture-adapter
  work rather than calibration heuristics.
- Replace KGlobalAccel launcher pairs with a press/release-capable portal or KDE
  adapter for true push-to-talk. Separate Start/Stop actions remain useful but
  are not push-to-talk.
- Fcitx `COMMIT2` now uses a bounded request ID, one safe retry, in-flight
  recipient replacement, and a cached terminal response. Lost ACKs therefore
  do not require duplicate dispatch. `CONTEXT` now exposes bounded surrounding
  text, cursor, selection, capabilities, truncation, and context generation for
  an explicit local cleanup action while rejecting sensitive fields. Proactive
  focus-loss notification remains future work.

### Keep as test doubles

- loopback HTTP servers, redirect/timeout/oversize responses, and sanitized
  provider JSON fixtures;
- synthetic PCM frames for VAD, trimming, WAV, and cancellation tests;
- the deterministic fixed-text provider when explicitly selected by a test;
- command children used to test timeout, cancellation, process-group cleanup,
  and bounded output;
- fake focus/secure-field outcomes and clipboard/Fcitx transport fixtures.

These doubles make boundary behavior reproducible and should not be replaced by
live services in the default test suite. Add opt-in live smoke tests separately.
