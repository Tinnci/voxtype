# Implementation layers and replacement plan

This document separates production behavior, deliberate test doubles, and
contracts that are not yet connected to the running daemon. A green mock flow
is never evidence of ASR quality or desktop delivery correctness.

## Layer 0: session policy and orchestration

Owns session IDs, state transitions, cancellation, provider routing, replay
consent, usage semantics, and stale-result rejection. The next production step
is to replace the daemon's synchronous `PreparedProvider` dispatch with a
bounded background job and connect the core router to real lifecycle states:
prepared, request started, audio accepted, completed, and cancelled.
Provider execution is now off the D-Bus object lock with session-checked result
delivery and cancellable curl/command children. Provider failures carry
transport-started plus conservative `NotAccepted`, `PossiblyAccepted`, or
`Accepted` evidence. Fallback and usage accounting no longer infer audio
exposure from the provider type; ambiguous cancellation or connection loss
requires explicit buffered replay consent.

## Layer 1: capture and speech signal processing

Owns devices, negotiated formats, resampling, bounded audio chunks, level and
clipping metrics, VAD, endpointing, and the recording spool. `parec` is a real
capture adapter, not a mock, but it remains a compatibility backend. The target
backend exposes PipeWire devices and stream failures directly while keeping the
provider-facing format at mono 16 kHz PCM.

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
The daemon emits `StateChanged(state, session)` lifecycle events. The tray uses
them for normal SNI icon/status and dbusmenu action updates, while a five-second
status read remains only as disconnect/reconnect protection. Plasma therefore
does not offer Start, Stop, and Cancel simultaneously.

## Layer 4: user experience, configuration, and diagnostics

Owns provider/profile onboarding, microphone selection, calibration, quota
labels, privacy disclosure, history consent, diagnostics, and accessibility.
The current grammar module is a real local typography normalizer, not a full
grammar model. It should be named and presented accurately; a future grammar
backend may use LanguageTool or another explicitly configured local/online
service without putting that dependency in the core daemon.

Audio calibration and `doctor audio` now report RMS, peak/clipping state, noise
floor, adaptive threshold, speech ratio, and a suggested threshold instead of
treating non-empty bytes as success. Device identity, startup latency, SNR,
and captured/dropped frame counters remain the next capture-adapter increment.
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

## Mock replacement inventory

Replace a double only when it can affect a normal user path. Keep deterministic
doubles at protocol and process boundaries so failure behavior remains testable.

### P0: remove from normal product paths

- A fixed-text provider must never be mistaken for successful ASR. A default
  demo-only profile is now rejected by the normal no-profile start action;
  developers may still name that profile explicitly for insertion integration
  tests. First-run UI must lead to a real cloud or local provider.
- The overlay must not advertise recent-text checking while transcript history
  is disabled. Retaining prior input requires an explicit privacy decision.
- Any provider `success` must come from a parsed non-empty provider response or
  a real local command result, never a generated sample string.

### P1: replace approximations with truthful production algorithms

- Evolve audio capture from the `parec` compatibility process to a PipeWire
  stream with negotiated device identity, bounded buffers, drop accounting,
  resampling, and hot-plug recovery.
- Keep the stateful energy VAD, but add DC rejection, independently smoothed
  short/long energy, calibrated SNR confidence, and live endpoint events. This
  is an algorithm improvement, not replacement of a mock.
- Present `grammar.rs` as local typography/text cleanup. A feature called full
semantic grammar checking needs a separate pluggable local or online checker plus a
  review/apply/undo flow; heuristics must not be labelled as semantic grammar.
- Replace the one-shot overlay snapshot with a persistent, event-driven view of
  daemon state, elapsed time, real input level, VAD state, and discrete provider
  phases. Decorative animation is not a level or completion measurement.
- Treat request/audio/token counters as daemon-session telemetry and configured
  limits as local soft limits. Provider account balance or billing quota may be
  shown only when fetched from an authoritative provider API with provenance.
- Replace calibration's single mixed sample with guided silence and speech
  phases that calculate noise, speech RMS, SNR, clipping ratio, and confidence.

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
