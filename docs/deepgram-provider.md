# Deepgram provider

VoxType integrates Deepgram through the official prerecorded speech-to-text
HTTP API. This is a distinct provider protocol, not an alias for the
OpenAI-compatible multipart adapter.

## Official interface

- API reference: <https://developers.deepgram.com/reference/speech-to-text/listen-pre-recorded>
- Default endpoint: `https://api.deepgram.com/v1/listen`
- Authentication: `Authorization: Token <API_KEY>`
- Request: binary WAV body with model, language, and smart-format query options
- Transcript: `results.channels[0].alternatives[0].transcript`

The endpoint remains configurable for regional gateways and loopback tests.
Remote plain HTTP is rejected; HTTP is permitted only for `localhost`, IPv4
loopback, or IPv6 loopback.

## Privacy and credentials

When a profile selects Deepgram, the captured microphone recording is sent to
Deepgram. Users must review Deepgram's current terms, retention controls, and
privacy policy for their account and region before enabling the provider.

The API key is stored by Secret Service/KWallet under an opaque reference. The
provider passes the authorization header to system `curl` through its private
stdin config stream. The key is absent from TOML, argv, normal errors, debug
formatting, transcript history, and notifications.

## Usage boundary

VoxType records local provider attempts, request-stage entries, successes,
failures, and submitted audio duration for the current daemon lifetime.
Deepgram's prerecorded response does not expose token counters, so VoxType shows
tokens as “not reported” and never derives a billing estimate from text length.
Account quotas and invoices remain authoritative only in Deepgram's service.

## Failure and fallback policy

- `401` and `403`: non-retryable authentication failure;
- `400`, `404`, and `422`: non-retryable request/protocol failure;
- `429`: retryable rate limit;
- transport and server failures: retryable unavailable state.

Even when a request fails, the service may already have received audio.
Replaying the same recording to another provider therefore requires the
profile's explicit `replay = "buffered-with-consent"` setting.

## Offline verification

Normal CI never contacts Deepgram. A loopback HTTP fixture verifies:

- exact `Token` authorization placement without exposing the key in argv;
- model and language query encoding;
- WAV binary upload;
- nested transcript parsing;
- HTTPS/loopback endpoint policy;
- malformed, empty, authentication, and retry-category behavior.
