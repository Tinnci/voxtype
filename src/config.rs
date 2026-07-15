//! XDG configuration and Secret Service integration.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use voxtype_core::{ErrorCategory, ReplayPolicy, VoxError};
use voxtype_provider_rest::SecretString;

const DEFAULT_CONFIG: &str = r#"schema_version = 1
default_profile = "test"

[desktop]
restore_clipboard = true
retain_recordings = false
insertion_backend = "fcitx"

[audio]
minimum_duration_millis = 250

[profiles.test]
primary = "mock"
fallbacks = []
language = "zh"
replay = "never"

[providers.mock]
kind = "mock"
text = "VoxType 本地集成测试"
"#;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub schema_version: u32,
    pub default_profile: String,
    #[serde(default)]
    pub desktop: DesktopConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    pub profiles: BTreeMap<String, ProfileConfig>,
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    pub restore_clipboard: bool,
    pub retain_recordings: bool,
    pub insertion_backend: InsertionBackend,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            restore_clipboard: true,
            retain_recordings: false,
            insertion_backend: InsertionBackend::Fcitx,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum InsertionBackend {
    #[default]
    Fcitx,
    Clipboard,
    Auto,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub minimum_duration_millis: u64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            minimum_duration_millis: 250,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProfileConfig {
    pub primary: String,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub replay: ReplaySetting,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
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

#[derive(Clone, Debug, Deserialize)]
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
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_timeout")]
        timeout_seconds: u64,
    },
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
            let timeout_seconds = match provider {
                ProviderConfig::OpenaiCompatible {
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
            if let ProviderConfig::Command { program, .. } = provider
                && program.trim().is_empty()
            {
                return Err(configuration("command provider program is empty"));
            }
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
    Ok(SecretString::new(value))
}

/// Stores a provider secret in Secret Service/KWallet.
///
/// # Errors
///
/// Returns an I/O or service error when the secret cannot be stored.
pub fn store_secret(name: &str, value: &[u8]) -> Result<(), VoxError> {
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
}
