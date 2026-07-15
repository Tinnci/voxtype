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
secret prompting will be added before the first stable release.

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
