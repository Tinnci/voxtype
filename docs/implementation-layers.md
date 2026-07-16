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
servers and synthetic PCM are correct test doubles and remain in CI. Required
hardening is a shared curl result containing exit code, HTTP status, bounded
body, and upload-start/audio-accepted state; redirects must not forward secrets
to another origin. Provider IDs, routing, cancellation, and usage types belong
to the core/application domain rather than one provider crate.

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

`copy` is the portable no-injection fallback. Clipboard plus `ydotool` remains
an explicit compatibility choice and is not a daemon service dependency.

## Layer 4: user experience, configuration, and diagnostics

Owns provider/profile onboarding, microphone selection, calibration, quota
labels, privacy disclosure, history consent, diagnostics, and accessibility.
The current grammar module is a real local typography normalizer, not a full
grammar model. It should be named and presented accurately; a future grammar
backend may use LanguageTool or another explicitly configured local/online
service without putting that dependency in the core daemon.

Audio calibration and `doctor audio` must replace byte-count success with
device identity, startup latency, RMS, SNR, clipping ratio, captured/dropped
frames, and actionable confidence. Session-local usage and soft quotas are not
provider billing balances and must remain labelled as such.

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
