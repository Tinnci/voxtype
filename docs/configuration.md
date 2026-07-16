# Configuration and providers

VoxType follows XDG paths. The default configuration is:

```text
~/.config/voxtype/config.toml
```

It is created with mode `0600`. Validate and reload it with:

```bash
voxtype config validate
voxtype reload
```

On Plasma 6, `voxtype-settings` provides the normal settings UI. It can edit
the default profile, insertion/VAD options, provider soft quotas, and API keys.
Provider endpoints and models remain visible in the panel and can always be
edited directly in TOML. API keys are never written to TOML or shown after
storage.

## Voice activity detection and trimming

The local VAD analyzes 20 ms PCM frames without an external DSP library. The
configured RMS threshold is an absolute lower bound; VoxType also estimates the
room noise floor from a low percentile and raises the effective threshold when
needed. Consecutive loud frames enter the speech state, while a release window
prevents short pauses from ending an utterance.

When speech is found, cloud and command providers receive a trimmed recording
with 160 ms of pre-roll and 300 ms of post-roll. This preserves fast consonant
onsets and natural endings while avoiding needless silence upload. The settings
panel can record a 2.5-second local calibration sample and show the noise floor,
dynamic threshold, peak, and speech ratio. Calibration audio is never uploaded
and is deleted immediately after analysis; suggested values are only applied
after explicit confirmation.

## Test profile

The development default uses a deterministic mock provider. It exercises real
microphone capture, state transitions, text insertion, clipboard restoration,
and cleanup without uploading audio:

```toml
[profiles.test]
primary = "mock"
fallbacks = []
language = "zh"
replay = "never"

[providers.mock]
kind = "mock"
text = "VoxType 本地集成测试"
```

Do not mistake a successful mock run for ASR quality verification.

## Local command providers

For a local model or an existing wrapper, use a command provider. VoxType starts
the program without a shell and exposes the captured raw 16 kHz mono s16le PCM and language through
environment variables. The command must print only the transcript to stdout.

```toml
[providers.local-whisper]
kind = "command"
program = "/usr/local/bin/voxtype-whisper-wrapper"
args = ["--read-environment"]
timeout_seconds = 120
```

The wrapper receives `VOXTYPE_AUDIO_PATH` and `VOXTYPE_LANGUAGE`; arguments are
not interpolated by VoxType, so the example wrapper should read the environment
variable directly rather than relying on `${...}` expansion. The command path must
be absolute and is terminated
when the configured timeout expires, and non-zero or empty output is treated as
a provider failure.

## OpenAI-compatible providers

Any service implementing the common multipart transcription shape can be an
independent provider instance:

```toml
[profiles.chinese-cloud]
primary = "cloud-a"
fallbacks = ["cloud-b"]
language = "zh"
replay = "never"

[providers.cloud-a]
kind = "openai-compatible"
endpoint = "https://provider-a.example/v1/audio/transcriptions"
model = "provider-model"
secret = "cloud-a-api-key"
timeout_seconds = 30

[providers.cloud-b]
kind = "openai-compatible"
endpoint = "https://provider-b.example/v1/audio/transcriptions"
model = "backup-model"
secret = "cloud-b-api-key"
timeout_seconds = 30
```

Store credentials through Secret Service/KWallet. The secret is read from
standard input and does not appear in process arguments or the TOML file:

```bash
printf '%s' "$API_KEY" | voxtype secret set cloud-a-api-key
```

Avoid placing the command in shell history with a literal key. Interactive
secret entry is also available in the settings panel.

## Deepgram provider

Deepgram uses its official prerecorded speech-to-text API and is a separate
protocol implementation rather than an OpenAI-compatible endpoint:

```toml
[profiles.deepgram-zh]
primary = "deepgram"
fallbacks = []
language = "zh"
replay = "never"

[providers.deepgram]
kind = "deepgram"
endpoint = "https://api.deepgram.com/v1/listen"
model = "nova-3"
secret = "deepgram-api-key"
timeout_seconds = 30
smart_format = true
```

Store `deepgram-api-key` from the settings panel or with `voxtype secret set`.
VoxType uploads a temporary WAV body with `Authorization: Token`; the key is
passed to `curl` through private stdin configuration and is not exposed in the
process arguments. See [Deepgram provider](deepgram-provider.md) for the API,
privacy, and failure boundary.

## Consumption and soft quotas

VoxType separates three different kinds of data:

- reliable session-local counters: provider attempts, request-stage entries,
  success/failure, and audio time;
- token counts explicitly returned by an API `usage` object;
- user-configured soft limits, which are not provider billing or account
  balances.

Configure limits per provider:

```toml
[quotas.cloud-a]
request_limit = 1000
audio_seconds_limit = 36000
token_limit = 1000000
```

Every limit is optional and must be positive. View the same data as JSON with
`voxtype usage`, or as progress meters in `voxtype-settings`. Counters currently
cover the lifetime of the running daemon and reset when it restarts; the panel
labels this scope explicitly.

Deepgram's prerecorded response does not provide token billing counters, so its
token value remains “not reported”. Request and audio-duration counters remain
available; VoxType never estimates tokens from transcript length.

## Fallback privacy policy

Recorded audio is sent to only the primary provider by default. For batch REST
APIs, an unsuccessful request may already have delivered audio, so fallback is
not attempted under `replay = "never"` or `before-audio-accepted`.

To permit replaying the same buffered recording to a second cloud provider, the
profile must explicitly state:

```toml
replay = "buffered-with-consent"
```

This increases privacy exposure, bandwidth, and possibly billing. Authentication
and configuration errors remain visible and are not hidden by fallback.

## Transport and dependency boundary

The REST provider uses the system `curl` executable so VoxType shares the
distribution-maintained TLS, proxy, and certificate stack. The API key is passed
through a private stdin configuration stream, never as a command argument.

Remote endpoints must use HTTPS. Plain HTTP is accepted only on loopback for
local integration tests and self-hosted development services.
