# ADR 0003: Single-writer application events and runtime wake

- Status: accepted
- Date: 2026-07-24

## Context

The initial daemon correctly serialized session mutations but also owned D-Bus,
capture, insertion, provider construction, routing, health, usage, and terminal
event storage. Provider results and lifecycle signals were delivered by a fixed
100 ms polling loop. This made provider additions modify the daemon directly
and spent most idle time polling while still adding up to 100 ms of avoidable
signal latency.

## Decision

Use `voxtype-app::AppController` as the only owner of:

- session state and accepted transitions;
- provider registry, route planning, replay enforcement, health and usage;
- bounded lifecycle events and transcript-free terminal results.

Concrete provider configuration is translated into `ProviderAdapter` objects
during startup or idle reload. Capture and insertion are injected through
runtime-neutral ports. A capacity-one `std::sync` wake channel coalesces bursts
without blocking or growing memory. D-Bus transitions and recognition workers
wake the daemon immediately. A timer remains only while recording for audio
telemetry and maximum-duration enforcement.

The default runtime stays thread-based. `voxtype-core` and `voxtype-app` expose
no Tokio, zbus, PipeWire, Qt, TLS, or provider-specific types.

## Consequences

- Adding a provider no longer changes recognition orchestration.
- Privacy-aware replay and usage evidence are tested once in `voxtype-app`.
- Fake capture and insertion adapters can exercise the complete session flow.
- A private Unix P2P D-Bus test covers `Start`, `Stop`, `SessionResult`, and
  terminal insertion without a desktop session or live cloud service.
- True live streaming still needs a bounded audio-source/event-sink extension;
  it does not require changing state ownership or adopting a global async
  runtime.

## Rejected alternatives

### Retain fixed polling

Simple, but adds avoidable latency to every state signal and recognition result
and wakes continuously while idle.

### Move the whole daemon to Tokio

Would add a runtime and async types before the single-session workload
demonstrates a need. Adapter-local async remains available later.

### Let workers mutate application state

Would weaken stale-session rejection and make lifecycle ordering dependent on
thread scheduling. Workers return bounded results; only `AppController` applies
state and accounting changes.
