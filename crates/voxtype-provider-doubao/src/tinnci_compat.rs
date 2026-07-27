//! Built-in compatibility profile for `Tinnci/doubao-asr-for-ha`.
//!
//! The profile intentionally remains labelled unofficial. It mirrors the
//! public upstream identity and protocol contract at a pinned revision, while
//! the transport, protobuf codec, persistence boundary, and error handling are
//! independently implemented in this crate.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use uuid::Uuid;
use voxtype_core::{ErrorCategory, VoxError};
use voxtype_provider_common::{CancellationToken, SecretString};

use crate::{
    BootstrapHttpConfig, BootstrapRequestContext, DeviceIdentity, RegisteredDevice,
    RegistrationDocument, register_device_http,
};

/// Upstream revision used to audit this compatibility profile.
pub const UPSTREAM_REVISION: &str = "ca4ed3ff8241d2824e21c90d7df3f1a463da6ed5";
pub const UPSTREAM_REPOSITORY: &str = "https://github.com/Tinnci/doubao-asr-for-ha";

const REGISTER_URL: &str = "https://log.snssdk.com/service/2/device_register/";
const SETTINGS_URL: &str = "https://is.snssdk.com/service/settings/v3/";
const WEBSOCKET_URL: &str = "wss://frontier-audio-ime-ws.doubao.com/ocean/api/v1/ws";
const AID: u32 = 401_734;
const APP_NAME: &str = "oime";
const VERSION_CODE: u32 = 100_311_008;
const VERSION_NAME: &str = "1.3.11";
const CHANNEL: &str = "official";
const PACKAGE: &str = "com.bytedance.android.doubaoime";
const DEVICE_PLATFORM: &str = "android";
const OS: &str = "android";
const OS_API: &str = "34";
const OS_VERSION: &str = "16";
const DEVICE_TYPE: &str = "Pixel 7 Pro";
const DEVICE_BRAND: &str = "google";
const DEVICE_MODEL: &str = "Pixel 7 Pro";
const RESOLUTION: &str = "1080*2400";
const DPI: &str = "420";
const LANGUAGE: &str = "zh";
const ACCESS: &str = "wifi";
const ROM: &str = "UP1A.231005.007";
const USER_AGENT: &str = concat!(
    "com.bytedance.android.doubaoime/100311008 ",
    "(Linux; U; Android 16; en_US; Pixel 7 Pro; ",
    "Build/BP2A.250605.031.A2; Cronet/TTNetVersion:94cf429a 2025-11-17 ",
    "QuicVersion:1f89f732 2025-05-08)"
);

/// Registers a fresh pseudonymous device and returns the complete managed
/// credential bundle for storage in Secret Service.
///
/// # Errors
///
/// Returns a redacted configuration, cancellation, transport, or upstream
/// protocol error. Generated identifiers never appear in diagnostics.
pub fn provision_managed_bundle(
    cancellation: &CancellationToken,
) -> Result<SecretString, VoxError> {
    let identity = generated_identity();
    let context = bootstrap_context(&identity);
    let document = registration_document(&identity)?;
    let registered = register_device_http(
        &BootstrapHttpConfig {
            registration_endpoint: REGISTER_URL.to_owned(),
            settings_endpoint: SETTINGS_URL.to_owned(),
            timeout_seconds: 15,
        },
        &context,
        &document,
        cancellation,
    )?;
    managed_bundle(&identity, &registered)
}

fn generated_identity() -> DeviceIdentity {
    DeviceIdentity {
        client_udid: Uuid::new_v4().to_string(),
        open_udid: Uuid::new_v4().simple().to_string()[..16].to_owned(),
        cdid: Uuid::new_v4().to_string(),
    }
}

fn bootstrap_context(identity: &DeviceIdentity) -> BootstrapRequestContext {
    BootstrapRequestContext {
        user_agent: USER_AGENT.to_owned(),
        common_query: vec![
            pair("device_platform", DEVICE_PLATFORM),
            pair("os", OS),
            pair("ssmix", "a"),
            pair("cdid", &identity.cdid),
            pair("channel", CHANNEL),
            pair("aid", &AID.to_string()),
            pair("app_name", APP_NAME),
            pair("version_code", &VERSION_CODE.to_string()),
            pair("version_name", VERSION_NAME),
            pair("manifest_version_code", &VERSION_CODE.to_string()),
            pair("update_version_code", &VERSION_CODE.to_string()),
            pair("resolution", RESOLUTION),
            pair("dpi", DPI),
            pair("device_type", DEVICE_TYPE),
            pair("device_brand", DEVICE_BRAND),
            pair("language", LANGUAGE),
            pair("os_api", OS_API),
            pair("os_version", OS_VERSION),
            pair("ac", ACCESS),
        ],
    }
}

fn registration_document(identity: &DeviceIdentity) -> Result<RegistrationDocument, VoxError> {
    identity.validate().map_err(|_| {
        VoxError::new(
            ErrorCategory::Internal,
            "doubao.identity_generation_failed",
            "Could not generate a valid Doubao compatibility identity",
        )
    })?;
    let generated_at = unix_millis()?;
    RegistrationDocument::from_json(&json!({
        "magic_tag": "ss_app_log",
        "header": {
            "device_id": 0,
            "install_id": 0,
            "aid": AID,
            "app_name": APP_NAME,
            "version_code": VERSION_CODE,
            "version_name": VERSION_NAME,
            "manifest_version_code": VERSION_CODE,
            "update_version_code": VERSION_CODE,
            "channel": CHANNEL,
            "package": PACKAGE,
            "device_platform": DEVICE_PLATFORM,
            "os": OS,
            "os_api": OS_API,
            "os_version": OS_VERSION,
            "device_type": DEVICE_TYPE,
            "device_brand": DEVICE_BRAND,
            "device_model": DEVICE_MODEL,
            "resolution": RESOLUTION,
            "dpi": DPI,
            "language": LANGUAGE,
            "timezone": 8,
            "access": ACCESS,
            "rom": ROM,
            "rom_version": ROM,
            "openudid": identity.open_udid,
            "clientudid": identity.client_udid,
            "cdid": identity.cdid,
            "region": "CN",
            "tz_name": "Asia/Shanghai",
            "tz_offset": 28800,
            "sim_region": "cn",
            "carrier_region": "cn",
            "cpu_abi": "arm64-v8a",
            "build_serial": "unknown",
            "not_request_sender": 0,
            "sig_hash": "",
            "google_aid": "",
            "mc": "",
            "serial_number": ""
        },
        "_gen_time": generated_at
    }))
    .map_err(|_| {
        VoxError::new(
            ErrorCategory::Internal,
            "doubao.registration_document_failed",
            "Could not build the Doubao registration request",
        )
    })
}

fn managed_bundle(
    identity: &DeviceIdentity,
    registered: &RegisteredDevice,
) -> Result<SecretString, VoxError> {
    let websocket_endpoint = websocket_endpoint(registered);
    let session = session_config(&registered.device_id);
    let value = json!({
        "schema": 1,
        "registration_endpoint": REGISTER_URL,
        "settings_endpoint": SETTINGS_URL,
        "user_agent": USER_AGENT,
        "common_query": bootstrap_context(identity).common_query,
        "device_id": registered.device_id,
        "install_id": registered.install_id,
        "websocket_endpoint": websocket_endpoint,
        "websocket_headers": [
            ["User-Agent", USER_AGENT],
            ["proto-version", "v2"],
            ["x-custom-keepalive", "true"]
        ],
        "session": session
    });
    let serialized = serde_json::to_string(&value).map_err(|_| {
        VoxError::new(
            ErrorCategory::Internal,
            "doubao.credential_bundle_failed",
            "Could not serialize the Doubao managed credential bundle",
        )
    })?;
    SecretString::try_new(serialized)
}

fn websocket_endpoint(registered: &RegisteredDevice) -> String {
    let params = [
        ("uid", "0"),
        ("aid", &AID.to_string()),
        ("app_name", APP_NAME),
        ("did", registered.device_id.as_str()),
        ("device_id", registered.device_id.as_str()),
        ("iid", registered.install_id.as_str()),
        ("install_id", registered.install_id.as_str()),
        ("channel", CHANNEL),
        ("os_version", OS_VERSION),
        ("version_code", &VERSION_CODE.to_string()),
        ("update_version_code", &VERSION_CODE.to_string()),
        ("version_name", VERSION_NAME),
        ("device_platform", DEVICE_PLATFORM),
        ("device_type", DEVICE_TYPE),
        ("device_brand", DEVICE_BRAND),
        ("ac", ACCESS),
        ("ip", "0"),
        ("user_agent", ""),
        ("forwarded", ""),
        ("target", ""),
        ("mobile", ""),
    ];
    let query = params
        .into_iter()
        .map(|(key, value)| format!("{key}={}", percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{WEBSOCKET_URL}?{query}")
}

fn session_config(device_id: &str) -> Value {
    json!({
        "audio_info": {
            "channel": 1,
            "format": "speech_opus",
            "sample_rate": 16000
        },
        "enable_punctuation": true,
        "enable_speech_rejection": false,
        "extra": {
            "aid": AID.to_string(),
            "app_name": APP_NAME,
            "cell_compress_rate": 8,
            "did": device_id,
            "enable_asr_threepass": true,
            "enable_asr_twopass": true,
            "input_mode": "tool",
            "update_version_code": VERSION_CODE.to_string(),
            "version_code": VERSION_CODE.to_string(),
            "version_name": VERSION_NAME
        }
    })
}

fn pair(key: &str, value: &str) -> (String, String) {
    (key.to_owned(), value.to_owned())
}

fn percent_encode(value: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn unix_millis() -> Result<u128, VoxError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|_| {
            VoxError::new(
                ErrorCategory::Internal,
                "doubao.system_clock_invalid",
                "System clock is before the Unix epoch",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::ManagedCredentialBundle;

    fn fixture_identity() -> DeviceIdentity {
        DeviceIdentity {
            client_udid: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
            open_udid: "0123456789abcdef".to_owned(),
            cdid: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_owned(),
        }
    }

    #[test]
    fn compatibility_bundle_round_trips_through_managed_parser() {
        let secret = managed_bundle(
            &fixture_identity(),
            &RegisteredDevice {
                device_id: "123456789".to_owned(),
                install_id: "987654321".to_owned(),
            },
        )
        .expect("compatibility bundle");
        ManagedCredentialBundle::parse(&secret).expect("managed parser accepts profile");
        assert!(!format!("{secret:?}").contains("123456789"));
    }

    #[test]
    fn websocket_profile_matches_audited_upstream_contract() {
        let endpoint = websocket_endpoint(&RegisteredDevice {
            device_id: "123".to_owned(),
            install_id: "456".to_owned(),
        });
        assert!(endpoint.starts_with(&format!("{WEBSOCKET_URL}?")));
        assert!(endpoint.contains("did=123"));
        assert!(endpoint.contains("install_id=456"));
        assert!(endpoint.contains("version_code=100311008"));
    }

    #[test]
    fn session_profile_requests_expected_audio_and_passes() {
        let profile = session_config("123");
        assert_eq!(profile["audio_info"]["format"], "speech_opus");
        assert_eq!(profile["audio_info"]["sample_rate"], 16_000);
        assert_eq!(profile["extra"]["did"], "123");
        assert_eq!(profile["extra"]["enable_asr_threepass"], true);
    }

    #[test]
    fn registration_profile_matches_pinned_upstream_identity() {
        let identity = fixture_identity();
        let context = bootstrap_context(&identity);
        assert_eq!(context.common_query.len(), 19);
        assert!(
            context
                .common_query
                .contains(&pair("version_code", "100311008"))
        );
        assert!(context.common_query.contains(&pair("cdid", &identity.cdid)));

        let document = registration_document(&identity).expect("registration document");
        let value: Value = serde_json::from_slice(&document.0).expect("registration document JSON");
        assert_eq!(value["magic_tag"], "ss_app_log");
        assert_eq!(value["header"]["aid"], AID);
        assert_eq!(value["header"]["package"], PACKAGE);
        assert_eq!(value["header"]["openudid"], identity.open_udid);
        assert_eq!(value["header"]["clientudid"], identity.client_udid);
        assert!(value["_gen_time"].as_u64().is_some());
    }
}
