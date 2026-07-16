//! XDG configuration and Secret Service integration.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use voxtype_core::{ErrorCategory, ReplayPolicy, VoxError};
use voxtype_provider_deepgram::{
    SecretString as DeepgramSecretString, validate_endpoint as validate_deepgram_endpoint,
};
use voxtype_provider_rest::{SecretString, validate_endpoint};

const DEFAULT_CONFIG: &str = r#"schema_version = 1
default_profile = "test"

[desktop]
restore_clipboard = true
retain_recordings = false
transcript_history_enabled = false
insertion_backend = "fcitx"

[audio]
minimum_duration_millis = 250
maximum_duration_seconds = 120
vad_enabled = true
vad_rms_threshold = 300
vad_minimum_voiced_frames = 2

[profiles.test]
primary = "mock"
fallbacks = []
language = "zh"
replay = "never"

[providers.mock]
kind = "mock"
text = "VoxType 本地集成测试"
"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub schema_version: u32,
    pub default_profile: String,
    #[serde(default)]
    pub desktop: DesktopConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    pub profiles: BTreeMap<String, ProfileConfig>,
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub quotas: BTreeMap<String, ProviderQuotaConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct DesktopConfig {
    pub restore_clipboard: bool,
    pub retain_recordings: bool,
    pub transcript_history_enabled: bool,
    pub insertion_backend: InsertionBackend,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            restore_clipboard: true,
            retain_recordings: false,
            transcript_history_enabled: false,
            insertion_backend: InsertionBackend::Fcitx,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InsertionBackend {
    #[default]
    Fcitx,
    Clipboard,
    Copy,
    Auto,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AudioConfig {
    pub minimum_duration_millis: u64,
    pub maximum_duration_seconds: u64,
    pub vad_enabled: bool,
    pub vad_rms_threshold: u16,
    pub vad_minimum_voiced_frames: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            minimum_duration_millis: 250,
            maximum_duration_seconds: 120,
            vad_enabled: true,
            vad_rms_threshold: 300,
            vad_minimum_voiced_frames: 2,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProfileConfig {
    pub primary: String,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub replay: ReplaySetting,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplaySetting {
    #[default]
    Never,
    BeforeAudioAccepted,
    BufferedWithConsent,
}

impl From<ReplaySetting> for ReplayPolicy {
    fn from(value: ReplaySetting) -> Self {
        match value {
            ReplaySetting::Never => Self::Never,
            ReplaySetting::BeforeAudioAccepted => Self::BeforeAudioAccepted,
            ReplaySetting::BufferedWithConsent => Self::BufferedWithConsent,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProviderConfig {
    Mock {
        text: String,
    },
    OpenaiCompatible {
        endpoint: String,
        model: String,
        secret: String,
        #[serde(default = "default_timeout")]
        timeout_seconds: u64,
    },
    Deepgram {
        endpoint: String,
        model: String,
        secret: String,
        #[serde(default = "default_timeout")]
        timeout_seconds: u64,
        #[serde(default = "default_true")]
        smart_format: bool,
    },
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_timeout")]
        timeout_seconds: u64,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderQuotaConfig {
    pub request_limit: Option<u64>,
    pub audio_seconds_limit: Option<u64>,
    pub token_limit: Option<u64>,
}

impl Config {
    /// Loads the user configuration, creating a safe test profile when absent.
    ///
    /// # Errors
    ///
    /// Returns a normalized error when the file cannot be read, created, parsed,
    /// or validated.
    pub fn load_or_create() -> Result<Self, VoxError> {
        let path = config_path()?;
        if !path.exists() {
            let parent = path.parent().ok_or_else(|| {
                VoxError::new(
                    ErrorCategory::Configuration,
                    "config.invalid_path",
                    "configuration path has no parent directory",
                )
            })?;
            fs::create_dir_all(parent).map_err(config_io)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(config_io)?;
            file.write_all(DEFAULT_CONFIG.as_bytes())
                .map_err(config_io)?;
        }
        let contents = fs::read_to_string(&path).map_err(config_io)?;
        let config: Self = toml::from_str(&contents).map_err(|error| {
            VoxError::new(
                ErrorCategory::Configuration,
                "config.parse_failed",
                format!("{}: {error}", path.display()),
            )
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Validates cross-references and safety limits.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for unsupported schema versions, missing
    /// providers/profiles, or unsafe timeout values.
    pub fn validate(&self) -> Result<(), VoxError> {
        if self.schema_version != 1 {
            return Err(configuration("unsupported schema_version"));
        }
        if !self.profiles.contains_key(&self.default_profile) {
            return Err(configuration("default_profile does not exist"));
        }
        for (name, profile) in &self.profiles {
            for provider in std::iter::once(&profile.primary).chain(&profile.fallbacks) {
                if !self.providers.contains_key(provider) {
                    return Err(configuration(&format!(
                        "profile {name} references unknown provider {provider}"
                    )));
                }
            }
        }
        for provider in self.providers.values() {
            match provider {
                ProviderConfig::Mock { text } if text.trim().is_empty() => {
                    return Err(configuration("mock provider text is empty"));
                }
                ProviderConfig::OpenaiCompatible { model, secret, .. }
                | ProviderConfig::Deepgram { model, secret, .. }
                    if model.trim().is_empty() || secret.trim().is_empty() =>
                {
                    return Err(configuration(
                        "cloud provider model and secret reference are required",
                    ));
                }
                _ => {}
            }
            let timeout_seconds = match provider {
                ProviderConfig::OpenaiCompatible {
                    timeout_seconds, ..
                }
                | ProviderConfig::Deepgram {
                    timeout_seconds, ..
                }
                | ProviderConfig::Command {
                    timeout_seconds, ..
                } => timeout_seconds,
                ProviderConfig::Mock { .. } => continue,
            };
            if !(1..=300).contains(timeout_seconds) {
                return Err(configuration(
                    "provider timeout must be between 1 and 300 seconds",
                ));
            }
            if let ProviderConfig::OpenaiCompatible { endpoint, .. } = provider {
                validate_endpoint(endpoint)?;
            }
            if let ProviderConfig::Deepgram { endpoint, .. } = provider {
                validate_deepgram_endpoint(endpoint)?;
            }
            if let ProviderConfig::Command { program, .. } = provider
                && program.trim().is_empty()
            {
                return Err(configuration("command provider program is empty"));
            }
            if let ProviderConfig::Command { program, .. } = provider
                && !std::path::Path::new(program).is_absolute()
            {
                return Err(configuration(
                    "command provider program must be an absolute path",
                ));
            }
        }
        for (provider, quota) in &self.quotas {
            if !self.providers.contains_key(provider) {
                return Err(configuration(&format!(
                    "quota references unknown provider {provider}"
                )));
            }
            if matches!(quota.request_limit, Some(0))
                || matches!(quota.audio_seconds_limit, Some(0))
                || matches!(quota.token_limit, Some(0))
            {
                return Err(configuration("provider quota limits must be positive"));
            }
        }
        if self.audio.vad_rms_threshold == 0 || self.audio.vad_rms_threshold > 10_000 {
            return Err(configuration(
                "VAD RMS threshold must be between 1 and 10000",
            ));
        }
        if !(5..=3_600).contains(&self.audio.maximum_duration_seconds) {
            return Err(configuration(
                "maximum recording duration must be between 5 and 3600 seconds",
            ));
        }
        if !(1..=100).contains(&self.audio.vad_minimum_voiced_frames) {
            return Err(configuration(
                "VAD minimum voiced frames must be between 1 and 100",
            ));
        }
        if !(50..=10_000).contains(&self.audio.minimum_duration_millis) {
            return Err(configuration(
                "minimum recording duration must be between 50 and 10000 milliseconds",
            ));
        }
        Ok(())
    }

    /// Writes a validated configuration atomically with user-only permissions.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when validation, serialization, or the
    /// atomic replacement fails.
    pub fn save(&self) -> Result<(), VoxError> {
        self.validate()?;
        let path = config_path()?;
        let parent = path
            .parent()
            .ok_or_else(|| configuration("configuration path has no parent directory"))?;
        fs::create_dir_all(parent).map_err(config_io)?;
        let serialized = toml::to_string_pretty(self).map_err(|error| {
            VoxError::new(
                ErrorCategory::Configuration,
                "config.serialize_failed",
                error.to_string(),
            )
        })?;
        let temporary = path.with_extension(format!("toml.tmp-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(config_io)?;
        if let Err(error) = file
            .write_all(serialized.as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, &path))
        {
            let _ = fs::remove_file(&temporary);
            return Err(config_io(error));
        }
        Ok(())
    }

    #[must_use]
    pub fn profile(&self, requested: Option<&str>) -> Option<(&str, &ProfileConfig)> {
        let name = requested
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.default_profile);
        self.profiles
            .get_key_value(name)
            .map(|(name, profile)| (name.as_str(), profile))
    }
}

/// Returns the XDG configuration path.
///
/// # Errors
///
/// Returns a configuration error when neither `XDG_CONFIG_HOME` nor `HOME` is
/// available.
pub fn config_path() -> Result<PathBuf, VoxError> {
    if let Some(directory) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(directory).join("voxtype/config.toml"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/voxtype/config.toml"))
        .ok_or_else(|| configuration("HOME and XDG_CONFIG_HOME are unavailable"))
}

/// Retrieves a provider secret from Secret Service/KWallet.
///
/// # Errors
///
/// Returns an authentication error if the secret does not exist or cannot be
/// retrieved.
pub fn lookup_secret(name: &str) -> Result<SecretString, VoxError> {
    lookup_secret_text(name).and_then(SecretString::try_new)
}

/// Retrieves a Deepgram credential from Secret Service/KWallet.
///
/// # Errors
///
/// Returns an authentication error if the secret does not exist or cannot be
/// retrieved.
pub fn lookup_deepgram_secret(name: &str) -> Result<DeepgramSecretString, VoxError> {
    lookup_secret_text(name).and_then(DeepgramSecretString::try_new)
}

fn lookup_secret_text(name: &str) -> Result<String, VoxError> {
    let output = Command::new("secret-tool")
        .args(["lookup", "application", "voxtype", "name", name])
        .output()
        .map_err(|error| {
            VoxError::new(
                ErrorCategory::Unavailable,
                "secret.service_unavailable",
                format!("could not start secret-tool: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err(VoxError::new(
            ErrorCategory::Authentication,
            "secret.not_found",
            format!("secret {name} is not available in Secret Service"),
        ));
    }
    let mut value = String::from_utf8(output.stdout).map_err(|_| {
        VoxError::new(
            ErrorCategory::Authentication,
            "secret.invalid_encoding",
            "stored secret is not valid UTF-8",
        )
    })?;
    while value.ends_with(['\n', '\r']) {
        value.pop();
    }
    if value.is_empty() {
        return Err(VoxError::new(
            ErrorCategory::Authentication,
            "secret.empty",
            format!("secret {name} is empty"),
        ));
    }
    Ok(value)
}

/// Checks whether a secret reference is available without loading its value
/// into the `VoxType` process.
#[must_use]
pub fn secret_state(name: &str) -> &'static str {
    match Command::new("secret-tool")
        .args(["lookup", "application", "voxtype", "name", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => "configured",
        Ok(_) => "missing",
        Err(_) => "unavailable",
    }
}

/// Stores a provider secret in Secret Service/KWallet.
///
/// # Errors
///
/// Returns an I/O or service error when the secret cannot be stored.
pub fn store_secret(name: &str, value: &[u8]) -> Result<(), VoxError> {
    voxtype_provider_common::validate_secret_bytes(value)?;
    let mut child = Command::new("secret-tool")
        .args([
            "store",
            &format!("--label=VoxType provider: {name}"),
            "application",
            "voxtype",
            "name",
            name,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(config_io)?;
    child
        .stdin
        .take()
        .ok_or_else(|| configuration("secret-tool stdin is unavailable"))?
        .write_all(value)
        .map_err(config_io)?;
    let output = child.wait_with_output().map_err(config_io)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(VoxError::new(
            ErrorCategory::Unavailable,
            "secret.store_failed",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

fn default_language() -> String {
    "zh".to_owned()
}

const fn default_timeout() -> u64 {
    30
}

const fn default_true() -> bool {
    true
}

fn config_io(error: io::Error) -> VoxError {
    let message = error.to_string();
    drop(error);
    VoxError::new(ErrorCategory::Configuration, "config.io_failed", message)
}

fn configuration(message: &str) -> VoxError {
    VoxError::new(
        ErrorCategory::Configuration,
        "config.invalid",
        message.to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_configuration() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config.validate().expect("default config validates");
        assert!(matches!(
            config.providers["mock"],
            ProviderConfig::Mock { .. }
        ));
    }

    #[test]
    fn rejects_missing_provider_reference() {
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config
            .profiles
            .get_mut("test")
            .expect("test profile")
            .primary = "missing".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_recording_safety_limit() {
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        assert_eq!(config.audio.maximum_duration_seconds, 120);
        config.audio.maximum_duration_seconds = 4;
        assert!(config.validate().is_err());
        config.audio.maximum_duration_seconds = 3_601;
        assert!(config.validate().is_err());
    }

    #[test]
    fn transcript_history_is_private_by_default() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        assert!(!config.desktop.transcript_history_enabled);
    }

    #[test]
    fn rejects_relative_command_provider_program() {
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config.providers.insert(
            "local".to_owned(),
            ProviderConfig::Command {
                program: "whisper-wrapper".to_owned(),
                args: Vec::new(),
                timeout_seconds: 30,
            },
        );
        config
            .profiles
            .get_mut("test")
            .expect("test profile")
            .primary = "local".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_remote_plain_http_provider_endpoint() {
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config.providers.insert(
            "cloud".to_owned(),
            ProviderConfig::OpenaiCompatible {
                endpoint: "http://example.com/v1/audio/transcriptions".to_owned(),
                model: "test".to_owned(),
                secret: "test".to_owned(),
                timeout_seconds: 30,
            },
        );
        config
            .profiles
            .get_mut("test")
            .expect("test profile")
            .primary = "cloud".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_positive_provider_quotas() {
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config.quotas.insert(
            "mock".to_owned(),
            ProviderQuotaConfig {
                request_limit: Some(100),
                audio_seconds_limit: Some(3_600),
                token_limit: Some(10_000),
            },
        );
        config.validate().expect("positive quota validates");
        config
            .quotas
            .get_mut("mock")
            .expect("mock quota")
            .token_limit = Some(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn serializes_configuration_without_secret_values() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        let serialized = toml::to_string_pretty(&config).expect("configuration serializes");
        assert!(serialized.contains("[providers.mock]"));
        assert!(!serialized.contains("api_key"));
    }

    #[test]
    fn round_trips_rest_provider_and_quota_settings() {
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config.providers.insert(
            "cloud".to_owned(),
            ProviderConfig::OpenaiCompatible {
                endpoint: "https://example.com/v1/audio/transcriptions".to_owned(),
                model: "asr-model".to_owned(),
                secret: "cloud-key".to_owned(),
                timeout_seconds: 45,
            },
        );
        config.quotas.insert(
            "cloud".to_owned(),
            ProviderQuotaConfig {
                request_limit: Some(100),
                audio_seconds_limit: Some(3_600),
                token_limit: Some(50_000),
            },
        );
        let serialized = toml::to_string_pretty(&config).expect("configuration serializes");
        let round_trip: Config = toml::from_str(&serialized).expect("configuration parses again");
        round_trip.validate().expect("round trip validates");
        assert_eq!(round_trip.quotas["cloud"].request_limit, Some(100));
        assert!(matches!(
            round_trip.providers["cloud"],
            ProviderConfig::OpenaiCompatible {
                timeout_seconds: 45,
                ..
            }
        ));
    }

    #[test]
    fn round_trips_deepgram_provider_settings() {
        let mut config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config.providers.insert(
            "deepgram".to_owned(),
            ProviderConfig::Deepgram {
                endpoint: "https://api.deepgram.com/v1/listen".to_owned(),
                model: "nova-3".to_owned(),
                secret: "deepgram-key".to_owned(),
                timeout_seconds: 45,
                smart_format: true,
            },
        );
        let serialized = toml::to_string_pretty(&config).expect("configuration serializes");
        let round_trip: Config = toml::from_str(&serialized).expect("configuration parses again");
        round_trip.validate().expect("round trip validates");
        assert!(matches!(
            round_trip.providers["deepgram"],
            ProviderConfig::Deepgram {
                timeout_seconds: 45,
                smart_format: true,
                ..
            }
        ));
    }
}
