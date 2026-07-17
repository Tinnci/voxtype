# Doubao ASR protocol analysis

## Status and constraints

The reference implementation uses endpoints and client identity associated with
the Doubao Android input method. This is an unofficial, undocumented integration,
not evidence of a public API contract. Endpoints, request fields, identifiers,
and behavior may change without notice.

No reference source has been copied into VoxType. Before implementation, the
licensing and provider terms described in the product requirements must be
resolved.

The clean-room implementation now lives in
`crates/voxtype-provider-doubao`. It contains only independently documented
wire behavior: bounded protobuf envelope encoding/decoding, unknown-field
skipping, 20 ms PCM frame reassembly/padding, and VAD/interim/final result JSON
interpretation. The bootstrap layer validates caller-supplied persistent IDs,
parses bounded registration/settings responses, zeroizes `app_key`, computes
the exact uppercase `x-ss-stub` MD5, and performs cancellable bounded HTTP calls
through the shared system-curl transport. Sensitive query values and request
bodies are supplied through curl stdin instead of process arguments. A safe
Rust wrapper around the system `libopus` encodes each exact 20 ms mono frame as
one raw Opus packet. A transport-independent session state machine now enforces
the task/session lifecycle, request-ID matching, non-zero status failures,
first/middle/last ordering, packet deduplication, cancellation terminality, and
the requirement that a non-empty final transcript arrive before
`SessionFinished`.

The crate deliberately contains no built-in production endpoint or Android
identity template. The caller must supply both under an explicit distribution
policy. WebSocket/TLS, daemon wiring, and live opt-in verification remain.
The root binary exposes a default-off `doubao-unofficial` build feature so this
protocol is not linked into normal distribution builds merely because the
workspace tests its isolated crate.

## Observed service flow

```text
generate local device IDs
        |
        v
POST device_register -> device_id + install_id
        |
        v
POST settings/v3 -> asr_config.app_key token
        |
        v
open WebSocket
        |
        v
StartTask -> TaskStarted
        |
        v
StartSession -> SessionStarted
        |
        +---- send 20 ms Opus TaskRequest frames ----+
        |                                             |
        +---- receive VAD / partial / final events <--+
        |
        v
last-frame marker + FinishSession -> SessionFinished
```

## HTTP endpoints observed upstream

### Device registration

- Method: `POST`
- URL: `https://log.snssdk.com/service/2/device_register/`
- User-Agent: Android client identity used by the reference project.
- Query: application version, `cdid`, display/device metadata, locale, OS, and
  network access fields. In the audited reference revision, `clientudid` and
  `openudid` occur in the JSON header rather than as query fields.
- JSON body: `magic_tag`, a detailed `header`, and millisecond generation time.
- Required response values: positive `device_id`; `install_id` is also stored.

Generated local identifiers include UUID-based `cdid`, `clientudid`, and a
16-character `openudid`. They are persistent credentials and must not be logged.

### ASR token/settings

- Method: `POST`
- URL: `https://is.snssdk.com/service/settings/v3/`
- Content body observed: `body=null`.
- Header observed: uppercase MD5 of the exact body in `x-ss-stub`.
- Query includes the registered `device_id` and common client metadata.
- Token response path observed: `data.settings.asr_config.app_key`.

The MD5 value here appears to be a request compatibility field, not a password
hash or a security guarantee. VoxType must not generalize it as authentication.

## WebSocket transport

- URL observed: `wss://frontier-audio-ime-ws.doubao.com/ocean/api/v1/ws`.
- Query includes device/application identity, network type, locale, and version.
  The audited reference revision has no separate per-connection `session_id`
  query field; the request ID is carried in the protobuf envelope.
- Headers include the observed Android User-Agent and frontier host metadata.
- Messages are binary protobuf wire-format records.
- The tiny observed schema uses only varints and length-delimited fields.

## Request envelope fields

| Field | Wire type | Meaning |
| --- | --- | --- |
| 2 | length-delimited | token |
| 3 | length-delimited | service name (`ASR`) |
| 5 | length-delimited | method name |
| 6 | length-delimited | JSON payload |
| 7 | length-delimited | encoded audio bytes |
| 8 | length-delimited | request ID |
| 9 | varint | audio frame state |

Observed method names are `StartTask`, `StartSession`, `TaskRequest`, and
`FinishSession`.

Frame state values observed:

- `1`: first frame
- `3`: middle frame
- `9`: last frame

The reference sends a 100-byte zero payload as the last-frame marker after at
least one real audio frame.

## Session configuration

The `StartSession` JSON payload observed requests:

- one channel;
- 16,000 Hz sample rate;
- `speech_opus` format;
- punctuation enabled;
- speech rejection disabled;
- two-pass and three-pass ASR enabled;
- input mode `tool`;
- registered device and client-version metadata.

The production codec is isolated in `opus_codec.rs`. Session tests may replace
it with a deterministic `OpusFrameEncoder`, avoiding codec-specific golden bytes
while the real provider still sends standards-compliant raw Opus packets. Codec
tests decode emitted packets back to exactly 320 samples instead of comparing
compressed output that can change between compatible libopus versions.

## Audio contract

- Capture/conversion input: mono signed 16-bit PCM at 16 kHz.
- Frame duration: 20 ms.
- PCM samples per frame: 320.
- PCM bytes per frame: 640.
- Transport payload: one independently encoded Opus frame per task request.
- Timestamp: session start milliseconds plus `frame_index * 20`.
- Sending and receiving must run concurrently to observe partial results and
  avoid upstream socket backpressure.

## Response envelope fields

| Field | Wire type | Meaning |
| --- | --- | --- |
| 1 | length-delimited | request ID |
| 2 | length-delimited | task ID |
| 3 | length-delimited | service name |
| 4 | length-delimited | message type |
| 5 | varint | status code |
| 6 | length-delimited | status message |
| 7 | length-delimited | result JSON |
| 9 | varint | observed unknown integer |

Message types observed by the reference adapter:

- `TaskStarted`
- `SessionStarted`
- `SessionFinished`
- `TaskFailed`
- `SessionFailed`

Result JSON is interpreted as:

- `extra.vad_start == true`: speech start event;
- no `results`: heartbeat/diagnostic event;
- `results[*].text`: current transcript;
- `results[*].is_interim == false`: non-interim result;
- `results[*].is_vad_finished == true`: provider endpoint detected;
- `results[*].extra.nonstream_result == true`: final non-stream result;
- `extra.packet_number`: provider result sequence.

A final result is observed when `nonstream_result` is true or when the result is
non-interim and VAD has finished. The session remains open until
`SessionFinished`.

The Rust session protocol preserves provider transcript bytes, ignores duplicate
or older packet numbers, and never lets a stale partial overwrite a newer final.
Building an audio request does not itself mark audio uploaded: the I/O layer must
explicitly confirm the successful socket write, which changes replay evidence
from `NotAccepted` to `PossiblyAccepted`. A matching provider result advances it
to `Accepted`.

## Error and retry policy

- Separate phases: credentials, connect, start task, start session, send audio,
  finish session, read transcript, and close.
- Apply a timeout to each awaited response, not only the total operation.
- On an authentication-like `StartTask` failure, refresh the token and retry the
  whole session at most once.
- Do not automatically replay after audio may have been accepted unless the
  provider behavior is proven idempotent.
- Redact token, device identifiers, query strings, headers, and raw provider
  payloads from errors.

## Implementation boundary for VoxType

Provider-specific code owns:

- device registration and token refresh;
- HTTP/WebSocket construction;
- protobuf envelope codec;
- JSON payload/result interpretation;
- Opus frame encoding and provider timestamps;
- mapping upstream errors to stable `ProviderError` categories.

The credential bootstrap boundary accepts a bounded serialized registration
document and exact common query metadata from a separately licensed
client-identity layer. The HTTP transport does not infer where a persistent ID
belongs. This keeps observed Android metadata out of the provider-neutral API
while allowing loopback tests to verify exact POST/query/header behavior without
contacting the service.

It must expose only the provider-neutral traits in `api-contracts.md`. No Android
identity constant, token, protobuf field number, or raw result JSON may escape
into the daemon state machine.

## Required fixtures and tests before live use

- Golden request bytes for all four methods, created under the chosen license.
- Unknown protobuf field skipping and malformed length/varint rejection.
- Task/session error messages and secret redaction.
- VAD-only, multiple partial, final, empty final, and reordered packet fixtures.
- Exactly 20 ms framing across arbitrary input chunk boundaries.
- Bounded queue behavior under a deliberately slow sender.
- Timeout and cancellation during every protocol phase.
- One token refresh retry and no infinite authentication loop.
- A live opt-in test that is excluded from normal CI and never records fixtures
  containing credentials or speech.
