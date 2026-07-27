# Doubao managed credential bundle

The unofficial Doubao provider is a default-compiled Tier 1 compatibility
target. `doubao-unofficial` remains as the config kind and Cargo feature name so
existing installations keep working. Non-secret TOML contains only a Secret
Service reference and timing policy:

```toml
[providers.doubao]
kind = "doubao-unofficial"
secret = "doubao-managed-bundle"
phase_timeout_seconds = 15
total_timeout_seconds = 180
frame_interval_millis = 20
```

The referenced Secret Service value is a compact, schema-versioned JSON object.
It is generated from the compatibility profile pinned to
`Tinnci/doubao-asr-for-ha` revision
`ca4ed3ff8241d2824e21c90d7df3f1a463da6ed5`:

```json
{"schema":1,"registration_endpoint":"https://…","settings_endpoint":"https://…","user_agent":"…","common_query":[["cdid","persistent-secret"]],"device_id":"persistent-secret","install_id":"persistent-secret","websocket_endpoint":"wss://…?device_id=persistent-secret","websocket_headers":[["proto-version","v2"]],"session":{"audio_info":{"channel":1,"sample_rate":16000,"format":"speech_opus"}}}
```

The redacted example is illustrative; do not hand-edit it. In Settings:

1. add a `doubao-unofficial` provider;
2. click **自动初始化兼容凭据**;
3. wait for device registration and Secret Service storage;
4. run the live provider doctor before relying on it for dictation.

Existing managed bundles can still be imported through the password-masked
field or:

```text
voxtype secret set doubao-managed-bundle
```

After configuring a profile that references the provider, run an explicit live
smoke test that captures from the configured microphone without arming Fcitx or
inserting text into the focused application:

```text
voxtype doctor provider-live doubao 6
```

The command requires an idle daemon, applies the normal local VAD trim, deletes
the temporary PCM file, and reports the final transcript and bounded transport
metrics. It is opt-in and is never run by normal tests or CI.

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

Automatic provisioning is deliberately explicit: opening Settings or creating a
provider does not contact the service. The initialization button generates three
local pseudonymous identifiers, registers one device, validates the resulting
bundle, then stores it with `secret-tool`. Reprovisioning is refused while a
bundle already exists so an accidental click cannot silently replace identity.

The compatibility identity is adapted from a PolyForm Noncommercial 1.0.0
source. Distribution or use that is not covered by that license requires a
separate license. See [Third-party notices](../THIRD_PARTY_NOTICES.md).
