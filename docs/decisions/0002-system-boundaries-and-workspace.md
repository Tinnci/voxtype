# ADR 0002: Provider boundaries and Cargo workspace

- Status: accepted
- Date: 2026-07-15
- Implemented: 2026-07-24

## Context

VoxType should support multiple online ASR providers. This reduces dependence on
one undocumented service, validates provider-neutral APIs, and lets users choose
based on availability, quality, privacy, latency, and cost.

Provider implementations can also carry very different dependency graphs,
credentials, licenses, and failure modes. Keeping every adapter in one large
package would make small core edits rebuild unrelated networking and native
code. Splitting every module into a package would create a different kind of
complexity without useful isolation.

## Decision

Use a small Cargo workspace once the second production provider begins:

- `voxtype-core`: dependency-light domain model, state policy,
  provider-neutral lifecycle evidence, and routing types;
- `voxtype-app`: daemon orchestration, the single runtime provider contract,
  registry, and composition ports;
- one package for each materially distinct provider;
- KDE desktop and audio packages when their native/heavy dependencies would
  otherwise affect core edit loops;
- thin CLI and daemon binaries that reuse the library packages.

Providers are linked at build time in version 0.x. Dynamic Rust plugins are not
part of the supported ABI. A provider may later become a separate worker process
when it needs strong crash, credential, licensing, or dependency isolation.

The implemented application package owns the session machine, provider
registry, route execution, health/usage accounting, terminal results, bounded
events, and capture/insertion ports. Concrete configuration, secrets, network
transports, D-Bus, PipeWire process capture, and KDE/Fcitx behavior remain in
outer adapters. A configuration enum is never dispatched in the recognition
hot path.

## Routing and fallback

- A profile selects one primary provider and zero or more ordered fallbacks.
- Only one provider receives audio by default.
- Fallback before any audio is accepted is safe when policy allows it.
- Replaying buffered audio to another cloud service requires explicit consent.
- Authentication/configuration errors remain visible rather than being hidden by
  fallback.
- Parallel racing, voting, and ensemble transcription are future opt-in modes
  because they multiply privacy exposure, cost, bandwidth, and complexity.

## Build reuse rationale

Rust/Cargo reuses compiled library artifacts and incremental code-generation
units, not arbitrary sections of linked binaries. Stable shared code therefore
belongs in library packages. Multiple binary targets can link the same already
compiled `rlib` artifacts when target/profile/features/toolchain match.

We will not introduce `dylib`/`cdylib` solely for compilation speed. The ABI and
deployment costs outweigh any benefit for this desktop application. Focused
package commands and optional `sccache` are the preferred accelerators.

## Consequences

- Core/provider API changes are explicit cross-package changes.
- Provider dependency and licensing risk stays localized.
- Core tests avoid linking audio, KDE, TLS, WebSocket, and codec dependencies.
- Full workspace builds remain available as a merge gate.
- There is some Cargo manifest and release-management overhead, so extraction is
  delayed until the second real provider makes the boundary valuable.

## Rejected alternatives

### Keep one package permanently

Simple initially, but provider and native adapter dependencies contaminate every
build and make feature/licensing boundaries harder to audit.

### One package for every module

Creates manifest churn and cross-crate API rigidity without proportional build
or ownership benefit.

### Rust dynamic plugins immediately

Rust has no stable native ABI for trait objects. A plugin SDK would require a C
ABI, serialization boundary, or dedicated framework and would complicate
distribution before third-party plugins are needed.

### One process per provider immediately

Provides strong isolation but adds IPC, lifecycle, packaging, logging, and audio
transfer complexity. Keep it as an escalation path for risky providers.
