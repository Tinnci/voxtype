# Doubao managed credential bundle

The unofficial Doubao provider is compiled only with the default-off
`doubao-unofficial` feature. Non-secret TOML contains only a Secret Service
reference and timing policy:

```toml
[providers.doubao]
kind = "doubao-unofficial"
secret = "doubao-managed-bundle"
phase_timeout_seconds = 15
total_timeout_seconds = 180
frame_interval_millis = 20
```

The referenced Secret Service value is a compact, single-line, schema-versioned
JSON object:

```json
{"schema":1,"registration_endpoint":"https://registration.example.invalid/service","settings_endpoint":"https://settings.example.invalid/service","user_agent":"licensed-client-profile","common_query":[["aid","licensed-value"],["cdid","persistent-secret"]],"device_id":"persistent-secret","install_id":"persistent-secret","websocket_endpoint":"wss://websocket.example.invalid/session?device_id=persistent-secret","websocket_headers":[["proto-version","licensed-value"],["x-custom-keepalive","licensed-value"]],"session":{"audio_info":{"channel":1,"sample_rate":16000,"format":"speech_opus"}}}
```

All endpoint/profile values above are placeholders. VoxType deliberately does
not ship the observed Android application identity, version, signature, device
fingerprint, or production endpoint defaults. A separately licensed identity
profile must create the exact bundle.

Store the compact JSON either through the feature-enabled settings panel or:

```text
voxtype secret set doubao-managed-bundle
```

Security properties:

- the bundle never belongs in TOML, command arguments, D-Bus state, logs, or
  diagnostics;
- `_rticket` is not stored in the bundle; VoxType generates the reserved Unix
  epoch millisecond value independently for every registration/settings call;
- `device_id`, `install_id`, `cdid`, WebSocket query values, and headers are
  treated as persistent pseudonymous credentials;
- the short-lived ASR token is fetched from the settings endpoint for each
  recognition and held in zeroizing memory;
- a StartTask authentication failure refreshes the token and retries once only
  before any audio upload;
- settings/token HTTP requests do not count as ASR requests or token usage;
- the service does not report LLM token consumption, so token usage remains
  unknown rather than zero.

The current bundle assumes an already registered device. Automatic identity
generation, device registration, and atomic Secret Service bundle replacement
remain separate work because they require an explicit licensing/distribution
decision for the client identity profile.
