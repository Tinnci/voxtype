# Dependency and build-time policy

## Goals

1. Fast incremental builds during desktop integration work.
2. A small auditable default binary and dependency graph.
3. Domain and protocol tests that do not link desktop/audio system libraries.
4. Replaceable adapters without leaking third-party types into core APIs.

## Current runtime strategy

The MVP starts with standard threads and bounded `std::sync` channels:

- `parec` stdout reader;
- synchronous provider request worker;
- serialized application state machine;
- blocking D-Bus adapter as required.

This workload permits only one active recognition session, so a general-purpose
async scheduler is not automatically justified. Tokio may be introduced behind
an adapter feature if measurements show a clear benefit. Core APIs must remain
runtime-neutral.

## Current dependency budget

| Capability | Current approach | Budget rule |
| --- | --- | --- |
| CLI parsing | standard library | no parser dependency at current command scale |
| Config | `serde` + parse-only `toml` | schema is owned by the application package |
| D-Bus | blocking `zbus`, default features disabled | one IPC/runtime stack |
| Audio | system `parec` through PipeWire-Pulse | no ALSA/PipeWire headers or build scripts |
| HTTP/TLS | system `curl` | reuse distribution TLS, proxy, and certificates |
| JSON | `serde_json` inside the REST provider package | no JSON in core domain APIs |
| Secrets | system `secret-tool` / Secret Service | secret material is not stored in TOML or argv |
| Desktop | D-Bus, Fcitx socket, and small system tools | no Qt linkage in Rust binaries |

## Dependency acceptance checklist

For every direct dependency record:

- exact responsibility and owning adapter;
- required Cargo features and disabled default features;
- minimum supported version and maintenance activity;
- license compatibility;
- unsafe/native code and build script behavior;
- number and weight of transitive dependencies;
- whether it adds a second runtime, TLS, HTTP, logging, or serialization stack;
- expected removal/replacement boundary;
- clean and incremental build impact.

Reject convenience dependencies whose functionality is a small, well-tested,
non-security-sensitive standard-library function. Accept specialized crates for
TLS, audio codecs, resampling, D-Bus, and secure memory rather than inventing
fragile implementations.

## Workspace layout

The current workspace intentionally has no feature matrix:

```text
voxtype                    application library + thin CLI/daemon/tray binaries
crates/voxtype-core        dependency-free domain policy and state machine
crates/voxtype-provider-rest  REST/WAV/JSON adapter
```

This keeps the fastest policy tests independent from `zbus`, TOML, JSON, and
desktop code while avoiding feature combinations that would duplicate Cargo
artifacts. A new package is justified only when a provider brings materially
different protocol, SDK, native build, or licensing risk.

## Crate/module strategy

- Start as one crate with modules while interfaces are still moving.
- Extract a dependency-light `voxtype-core` before adding the second real online
  provider. It becomes the shared policy and provider-contract package.
- Put each provider with materially different SDK, TLS, codec, licensing, or
  protocol risk in its own package.
- Keep providers using the same small transport stack separate at the package
  boundary, but share only proven common helpers such as redaction and contract
  tests. Do not create a speculative universal cloud client.
- Extract desktop/audio native adapters when doing so lets `cargo test -p
  voxtype-core` avoid their build scripts and system libraries.
- Do not create a crate per trait or layer.
- Avoid a stable Rust dynamic plugin ABI in 0.x; process-isolated providers are
  safer if third-party plugins become necessary.

## What Rust can reuse during compilation

Cargo does not normally reuse pieces of an already linked executable. Reuse
happens one level earlier:

- each library package is compiled to reusable metadata and `rlib` artifacts;
- Cargo reuses unchanged artifacts for the same target, profile, feature set,
  compiler, and relevant flags;
- multiple binaries in one workspace can share the same compiled library
  artifacts instead of compiling the source independently;
- incremental compilation reuses unchanged code-generation units inside a
  package during local edits;
- `sccache` can reuse compilation outputs across clean worktrees or CI jobs.

Therefore shared code should live in library packages, and `voxtype`/`voxtyped`
binary targets should be thin. A `cdylib` or system shared object is not a build
speed tool here: it introduces ABI, deployment, optimization, and debugging
costs. Rust dynamic libraries are justified only for a runtime plugin boundary,
not to make ordinary builds faster.

Artifact reuse has exact cache keys. Changing features, `RUSTFLAGS`, target,
profile, compiler version, or dependency versions can cause another compilation.
Workspace commands should keep those inputs consistent. Cargo feature unification
also means a broad `--all-features` build may compile heavier variants than a
focused package check.

## Recommended developer loops

Use these loops now:

```bash
# Fast policy/API loop; skips desktop, audio, TLS, and codecs.
cargo test -p voxtype-core

# REST provider and its loopback protocol test only.
cargo test -p voxtype-provider-rest

# Application type-check without linking release binaries.
cargo check -p voxtype

# Full integration only before merging.
cargo test --workspace
```

Keep CLI and daemon entry points in one lightweight package when they use the
same application libraries. Separate provider worker binaries only when fault,
license, credential, or dependency isolation outweighs IPC and packaging cost.

## Compile-time practices

- Keep default features narrow and set `default-features = false` where audited.
- Avoid proc macros in frequently edited core code.
- Put heavy adapters behind feature gates so domain test cycles skip them.
- Keep generated bindings checked in only when their tool/license permits and
  regeneration is deterministic; otherwise isolate the build script.
- Use `cargo check` for edit loops and link only relevant test targets.
- Commit `Cargo.lock` and pin CI toolchains intentionally.
- Use rust-lld or mold only as an optional developer configuration, never a
  repository default that breaks unsupported distributions.

Repository profiles currently use line-table debug info for dev/test and retain
incremental compilation. Developers can use `--profile debug-full` when full
debug information is needed. Release uses thin LTO and symbol stripping; release
build speed is secondary to local iteration speed.

## Measurement commands

Baseline these after the first real adapters land:

```bash
cargo clean && /usr/bin/time -v cargo check -p voxtype-core
/usr/bin/time -v cargo check -p voxtype
/usr/bin/time -v cargo test -p voxtype-provider-rest
cargo tree -d
cargo tree -e features
cargo build --release
stat -c '%s' target/release/voxtype
```

Optional developer accelerators:

- `sccache` for repeated clean builds and CI;
- `mold` or `rust-lld` after a local availability check;
- `cargo-nextest` only if test-suite scale justifies another tool;
- separate CI jobs for dependency audit instead of burdening normal edit loops.

## Initial build targets

- Core check: no default features and no native desktop/audio linkage.
- KDE check: desktop feature with D-Bus metadata validation.
- Provider codec check: offline fixtures only.
- Full Linux check: selected audio, desktop, and provider adapters.
- Live provider tests: manual/secret-gated, never required for pull requests.
