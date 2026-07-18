//! Secret-Service credential bundle and managed token bootstrap.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use voxtype_core::{AudioAcceptance, ErrorCategory, ProviderAttemptFailure, VoxError};
use voxtype_provider_common::{CancellationToken, SecretString};

use crate::runner::{DoubaoRunConfig, DoubaoTranscription, transcribe_pcm_with_token_refresh};
use crate::websocket::WebSocketSpec;
use crate::{
    BootstrapHttpConfig, BootstrapRequestContext, RegisteredDevice, fetch_settings_token_http,
};

const MAX_CREDENTIAL_BUNDLE_BYTES: usize = 256 * 1024;
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Parsed managed credentials. All fields are redacted from `Debug`.
pub struct ManagedCredentialBundle {
    bootstrap: BootstrapHttpConfig,
    context: BootstrapRequestContext,
    registered: RegisteredDevice,
    websocket_endpoint: String,
    websocket_headers: Vec<(String, String)>,
    session_json: Vec<u8>,
}

impl std::fmt::Debug for ManagedCredentialBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ManagedCredentialBundle([redacted])")
    }
}

impl ManagedCredentialBundle {
    /// Parses a versioned compact JSON value retrieved from Secret Service.
    ///
    /// # Errors
    ///
    /// Rejects oversized, malformed, unsupported, empty, or control-containing
    /// credential fields. Raw identifiers are never copied into the error.
    pub fn parse(secret: &SecretString) -> Result<Self, VoxError> {
        if secret.expose().len() > MAX_CREDENTIAL_BUNDLE_BYTES {
            return Err(configuration(
                "doubao.credential_bundle_too_large",
                "Doubao credential bundle is too large",
            ));
        }
        let raw: RawBundle = serde_json::from_str(secret.expose()).map_err(|_| {
            configuration(
                "doubao.credential_bundle_invalid_json",
                "Doubao credential bundle is not valid JSON",
            )
        })?;
        if raw.schema != 1 {
            return Err(configuration(
                "doubao.credential_bundle_schema",
                "Doubao credential bundle schema is unsupported",
            ));
        }
        validate_text(&raw.registration_endpoint, 4096)?;
        validate_text(&raw.settings_endpoint, 4096)?;
        validate_text(&raw.user_agent, 512)?;
        validate_text(&raw.device_id, 128)?;
        if !raw.install_id.is_empty() {
            validate_text(&raw.install_id, 128)?;
        }
        validate_text(&raw.websocket_endpoint, 16 * 1024)?;
        if raw.common_query.len() > 64 || raw.websocket_headers.len() > 32 {
            return Err(configuration(
                "doubao.credential_bundle_fields",
                "Doubao credential bundle contains too many metadata fields",
            ));
        }
        for pair in raw.common_query.iter().chain(&raw.websocket_headers) {
            validate_text(&pair[0], 64)?;
            validate_text(&pair[1], 1024)?;
        }
        if !raw.session.is_object() {
            return Err(configuration(
                "doubao.credential_session_invalid",
                "Doubao session profile must be a JSON object",
            ));
        }
        let session_json = serde_json::to_vec(&raw.session).map_err(|_| {
            configuration(
                "doubao.credential_session_invalid",
                "Could not serialize the Doubao session profile",
            )
        })?;
        Ok(Self {
            bootstrap: BootstrapHttpConfig {
                registration_endpoint: raw.registration_endpoint,
                settings_endpoint: raw.settings_endpoint,
                timeout_seconds: 15,
            },
            context: BootstrapRequestContext {
                user_agent: raw.user_agent,
                common_query: pairs(raw.common_query),
            },
            registered: RegisteredDevice {
                device_id: raw.device_id,
                install_id: raw.install_id,
            },
            websocket_endpoint: raw.websocket_endpoint,
            websocket_headers: pairs(raw.websocket_headers),
            session_json,
        })
    }
}

#[derive(Debug)]
pub struct ManagedDoubaoConfig {
    pub credentials: ManagedCredentialBundle,
    pub phase_timeout: Duration,
    pub total_timeout: Duration,
    pub frame_interval: Duration,
}

/// Fetches a short-lived token, runs recognition, and refreshes once on an
/// authentication-like `StartTask` failure before audio upload.
///
/// # Errors
///
/// Returns structured bootstrap or recognition evidence. Bootstrap HTTP
/// failures count as a started provider attempt but never as accepted audio.
pub fn transcribe_managed_pcm(
    config: &ManagedDoubaoConfig,
    pcm_path: &std::path::Path,
    cancellation: &CancellationToken,
) -> Result<DoubaoTranscription, ProviderAttemptFailure> {
    let initial = fetch_settings_token_http(
        &config.credentials.bootstrap,
        &config.credentials.context,
        &config.credentials.registered,
        cancellation,
    )
    .map_err(bootstrap_failure)?;
    let run = DoubaoRunConfig {
        websocket: WebSocketSpec {
            endpoint: config.credentials.websocket_endpoint.clone(),
            headers: config.credentials.websocket_headers.clone(),
            connect_timeout: config.phase_timeout,
            poll_interval: Duration::from_millis(10),
        },
        request_id: request_id()?,
        session_json: config.credentials.session_json.clone(),
        phase_timeout: config.phase_timeout,
        total_timeout: config.total_timeout,
        frame_interval: config.frame_interval,
    };
    transcribe_pcm_with_token_refresh(&run, &initial, pcm_path, cancellation, |cancel| {
        fetch_settings_token_http(
            &config.credentials.bootstrap,
            &config.credentials.context,
            &config.credentials.registered,
            cancel,
        )
    })
}

#[derive(Deserialize)]
struct RawBundle {
    schema: u32,
    registration_endpoint: String,
    settings_endpoint: String,
    user_agent: String,
    #[serde(default)]
    common_query: Vec<[String; 2]>,
    device_id: String,
    #[serde(default)]
    install_id: String,
    websocket_endpoint: String,
    #[serde(default)]
    websocket_headers: Vec<[String; 2]>,
    session: serde_json::Value,
}

fn pairs(values: Vec<[String; 2]>) -> Vec<(String, String)> {
    values
        .into_iter()
        .map(|[name, value]| (name, value))
        .collect()
}

fn validate_text(value: &str, maximum: usize) -> Result<(), VoxError> {
    if value.is_empty()
        || value.len() > maximum
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        Err(configuration(
            "doubao.credential_bundle_field",
            "Doubao credential bundle contains an invalid field",
        ))
    } else {
        Ok(())
    }
}

fn request_id() -> Result<String, ProviderAttemptFailure> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            ProviderAttemptFailure::before_transport(VoxError::new(
                ErrorCategory::Internal,
                "doubao.system_clock_invalid",
                "System clock is before the Unix epoch",
            ))
        })?
        .as_millis();
    Ok(format!(
        "voxtype-{millis}-{}",
        NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn bootstrap_failure(error: VoxError) -> ProviderAttemptFailure {
    if error.category() == ErrorCategory::Configuration {
        ProviderAttemptFailure::before_transport(error)
    } else {
        ProviderAttemptFailure::after_transport(error, AudioAcceptance::NotAccepted)
    }
}

fn configuration(code: &'static str, message: &'static str) -> VoxError {
    VoxError::new(ErrorCategory::Configuration, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_json() -> String {
        serde_json::json!({
            "schema": 1,
            "registration_endpoint": "https://registration.invalid/service",
            "settings_endpoint": "https://settings.invalid/service",
            "user_agent": "fixture-agent",
            "common_query": [["aid", "fixture"], ["cdid", "secret-cdid"]],
            "device_id": "secret-device-id",
            "install_id": "secret-install-id",
            "websocket_endpoint": "wss://websocket.invalid/session?device_id=secret-device-id",
            "websocket_headers": [["proto-version", "1"], ["x-custom-keepalive", "1"]],
            "session": {"audio_info": {"sample_rate": 16000, "format": "speech_opus"}}
        })
        .to_string()
    }

    #[test]
    fn parses_versioned_bundle_without_debug_exposure() {
        let secret = SecretString::try_new(bundle_json()).expect("bundle secret");
        let bundle = ManagedCredentialBundle::parse(&secret).expect("credential bundle");
        let debug = format!("{bundle:?}");
        assert!(!debug.contains("secret-device-id"));
        assert!(!debug.contains("fixture-agent"));
    }

    #[test]
    fn rejects_unknown_schema_and_control_bytes() {
        let mut value: serde_json::Value =
            serde_json::from_str(&bundle_json()).expect("bundle JSON");
        value["schema"] = serde_json::json!(2);
        let secret = SecretString::try_new(value.to_string()).expect("bundle secret");
        assert_eq!(
            ManagedCredentialBundle::parse(&secret)
                .expect_err("unknown schema")
                .code(),
            "doubao.credential_bundle_schema"
        );

        value["schema"] = serde_json::json!(1);
        value["device_id"] = serde_json::json!("bad\nvalue");
        let secret = SecretString::try_new(value.to_string()).expect("escaped bundle secret");
        assert_eq!(
            ManagedCredentialBundle::parse(&secret)
                .expect_err("control byte")
                .code(),
            "doubao.credential_bundle_field"
        );
    }
}
