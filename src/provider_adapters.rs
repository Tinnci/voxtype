//! Composition of configured provider adapters.
//!
//! Configuration and secret references are translated into the single
//! `voxtype-app` provider contract here. The daemon never dispatches on a
//! provider-specific enum during recognition.

use crate::config::{Config, ProviderConfig, lookup_deepgram_secret, lookup_secret};
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use voxtype_app::{
    ProviderAdapter, ProviderRegistry, ProviderSuccess, ProviderTranscript, ProviderUsage,
    RecognitionInput,
};
use voxtype_core::{
    AudioAcceptance, AudioFormat, CancellationToken, ErrorCategory, ProviderAttemptFailure,
    ProviderCapabilities, ProviderId, VoxError,
};
use voxtype_provider_deepgram::{
    DeepgramConfig, transcribe_pcm_with_evidence as transcribe_deepgram_pcm,
};
#[cfg(feature = "doubao-unofficial")]
use voxtype_provider_doubao::managed::{
    ManagedCredentialBundle, ManagedDoubaoConfig, transcribe_managed_pcm,
};
use voxtype_provider_rest::{
    RestProviderConfig, transcribe_pcm_with_evidence as transcribe_rest_pcm,
};

/// Builds the provider registry once during daemon composition or idle reload.
///
/// # Errors
///
/// Returns a configuration error for an invalid/duplicate provider ID.
pub fn build_provider_registry(config: &Config) -> Result<ProviderRegistry, VoxError> {
    let mut registry = ProviderRegistry::new();
    for (name, provider) in &config.providers {
        let id = ProviderId::new(name.clone())?;
        let adapter: Arc<dyn ProviderAdapter> = match provider {
            ProviderConfig::Mock { text } => Arc::new(MockAdapter {
                id,
                text: text.clone(),
            }),
            ProviderConfig::OpenaiCompatible {
                endpoint,
                model,
                secret,
                timeout_seconds,
            } => Arc::new(RestAdapter {
                id,
                endpoint: endpoint.clone(),
                model: model.clone(),
                secret: secret.clone(),
                timeout_seconds: *timeout_seconds,
            }),
            ProviderConfig::Deepgram {
                endpoint,
                model,
                secret,
                timeout_seconds,
                smart_format,
            } => Arc::new(DeepgramAdapter {
                id,
                endpoint: endpoint.clone(),
                model: model.clone(),
                secret: secret.clone(),
                timeout_seconds: *timeout_seconds,
                smart_format: *smart_format,
            }),
            #[cfg(feature = "doubao-unofficial")]
            ProviderConfig::DoubaoUnofficial {
                secret,
                phase_timeout_seconds,
                total_timeout_seconds,
                frame_interval_millis,
            } => Arc::new(DoubaoAdapter {
                id,
                secret: secret.clone(),
                phase_timeout: Duration::from_secs(*phase_timeout_seconds),
                total_timeout: Duration::from_secs(*total_timeout_seconds),
                frame_interval: Duration::from_millis(*frame_interval_millis),
            }),
            ProviderConfig::Command {
                program,
                args,
                timeout_seconds,
            } => Arc::new(CommandAdapter {
                id,
                program: program.clone(),
                args: args.clone(),
                timeout_seconds: *timeout_seconds,
            }),
        };
        registry.register(adapter)?;
    }
    Ok(registry)
}

struct MockAdapter {
    id: ProviderId,
    text: String,
}

impl ProviderAdapter for MockAdapter {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        recorded_pcm_capabilities()
    }

    fn recognize(
        &self,
        _input: RecognitionInput<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProviderSuccess, ProviderAttemptFailure> {
        ensure_not_cancelled(cancellation)?;
        if self.text.trim().is_empty() {
            return Err(ProviderAttemptFailure::before_transport(VoxError::new(
                ErrorCategory::Protocol,
                "provider.mock_empty",
                "mock provider text is empty",
            )));
        }
        Ok(success_without_transport(
            self.text.clone(),
            ProviderUsage::default(),
        ))
    }
}

struct RestAdapter {
    id: ProviderId,
    endpoint: String,
    model: String,
    secret: String,
    timeout_seconds: u64,
}

impl ProviderAdapter for RestAdapter {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        recorded_pcm_capabilities()
    }

    fn recognize(
        &self,
        input: RecognitionInput<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProviderSuccess, ProviderAttemptFailure> {
        ensure_not_cancelled(cancellation)?;
        let api_key =
            lookup_secret(&self.secret).map_err(ProviderAttemptFailure::before_transport)?;
        let config = RestProviderConfig {
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            api_key,
            timeout_seconds: self.timeout_seconds,
        };
        transcribe_rest_pcm(&config, input.pcm_path, input.language, cancellation).map(|result| {
            success_after_audio(
                result.text,
                ProviderUsage {
                    input_tokens: result.usage.input_tokens,
                    output_tokens: result.usage.output_tokens,
                    total_tokens: result.usage.total_tokens,
                },
            )
        })
    }
}

struct DeepgramAdapter {
    id: ProviderId,
    endpoint: String,
    model: String,
    secret: String,
    timeout_seconds: u64,
    smart_format: bool,
}

impl ProviderAdapter for DeepgramAdapter {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        recorded_pcm_capabilities()
    }

    fn recognize(
        &self,
        input: RecognitionInput<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProviderSuccess, ProviderAttemptFailure> {
        ensure_not_cancelled(cancellation)?;
        let api_key = lookup_deepgram_secret(&self.secret)
            .map_err(ProviderAttemptFailure::before_transport)?;
        let config = DeepgramConfig {
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            api_key,
            timeout_seconds: self.timeout_seconds,
            smart_format: self.smart_format,
        };
        transcribe_deepgram_pcm(&config, input.pcm_path, input.language, cancellation)
            .map(|result| success_after_audio(result.text, ProviderUsage::default()))
    }
}

#[cfg(feature = "doubao-unofficial")]
struct DoubaoAdapter {
    id: ProviderId,
    secret: String,
    phase_timeout: Duration,
    total_timeout: Duration,
    frame_interval: Duration,
}

#[cfg(feature = "doubao-unofficial")]
impl ProviderAdapter for DoubaoAdapter {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // The underlying protocol is streaming, but this adapter currently
        // consumes a completed recording and exposes only a final result.
        recorded_pcm_capabilities()
    }

    fn recognize(
        &self,
        input: RecognitionInput<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProviderSuccess, ProviderAttemptFailure> {
        ensure_not_cancelled(cancellation)?;
        let secret =
            lookup_secret(&self.secret).map_err(ProviderAttemptFailure::before_transport)?;
        let credentials = ManagedCredentialBundle::parse(&secret)
            .map_err(ProviderAttemptFailure::before_transport)?;
        let config = ManagedDoubaoConfig {
            credentials,
            phase_timeout: self.phase_timeout,
            total_timeout: self.total_timeout,
            frame_interval: self.frame_interval,
        };
        transcribe_managed_pcm(&config, input.pcm_path, cancellation)
            .map(|result| success_after_audio(result.text, ProviderUsage::default()))
    }
}

struct CommandAdapter {
    id: ProviderId,
    program: String,
    args: Vec<String>,
    timeout_seconds: u64,
}

impl ProviderAdapter for CommandAdapter {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        recorded_pcm_capabilities()
    }

    fn recognize(
        &self,
        input: RecognitionInput<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProviderSuccess, ProviderAttemptFailure> {
        transcribe_command_with_evidence(
            &self.program,
            &self.args,
            self.timeout_seconds,
            input.pcm_path,
            input.language,
            cancellation,
        )
        .map(|text| success_after_audio(text, ProviderUsage::default()))
    }
}

fn recorded_pcm_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        languages: Vec::new(),
        accepted_formats: vec![AudioFormat::PCM_16KHZ_MONO],
        streaming: false,
        partial_results: false,
        provider_vad: false,
    }
}

fn success_after_audio(text: String, usage: ProviderUsage) -> ProviderSuccess {
    ProviderSuccess {
        transcript: ProviderTranscript { text, usage },
        transport_started: true,
        audio_acceptance: AudioAcceptance::Accepted,
    }
}

fn success_without_transport(text: String, usage: ProviderUsage) -> ProviderSuccess {
    ProviderSuccess {
        transcript: ProviderTranscript { text, usage },
        transport_started: false,
        audio_acceptance: AudioAcceptance::NotAccepted,
    }
}

fn ensure_not_cancelled(cancellation: &CancellationToken) -> Result<(), ProviderAttemptFailure> {
    if cancellation.is_cancelled() {
        Err(ProviderAttemptFailure::before_transport(cancelled_error()))
    } else {
        Ok(())
    }
}

fn transcribe_command_with_evidence(
    program: &str,
    args: &[String],
    timeout_seconds: u64,
    pcm_path: &std::path::Path,
    language: &str,
    cancellation: &CancellationToken,
) -> Result<String, ProviderAttemptFailure> {
    ensure_not_cancelled(cancellation)?;
    let mut child = Command::new(program)
        .args(args)
        .env("VOXTYPE_AUDIO_PATH", pcm_path)
        .env("VOXTYPE_LANGUAGE", language)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| {
            ProviderAttemptFailure::before_transport(
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "provider.command_failed",
                    error.to_string(),
                )
                .with_retryable(true),
            )
        })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Internal,
                "provider.command_output",
                "command provider stdout is unavailable",
            ),
            AudioAcceptance::PossiblyAccepted,
        )
    })?;
    let output_reader = thread::spawn(move || read_command_output(stdout, 1024 * 1024));
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let status = match wait_for_command(&mut child, deadline, cancellation) {
        Ok(status) => status,
        Err(error) => {
            let _result = output_reader.join();
            return Err(error);
        }
    };
    let output = output_reader
        .join()
        .map_err(|_| {
            ProviderAttemptFailure::after_transport(
                VoxError::new(
                    ErrorCategory::Internal,
                    "provider.command_output",
                    "command output reader panicked",
                ),
                AudioAcceptance::Accepted,
            )
        })?
        .map_err(|error| {
            ProviderAttemptFailure::after_transport(
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "provider.command_output",
                    error.to_string(),
                ),
                AudioAcceptance::Accepted,
            )
        })?;
    finish_command_output(status, output)
}

fn wait_for_command(
    child: &mut std::process::Child,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<std::process::ExitStatus, ProviderAttemptFailure> {
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            ProviderAttemptFailure::after_transport(
                VoxError::new(
                    ErrorCategory::Unavailable,
                    "provider.command_wait",
                    error.to_string(),
                ),
                AudioAcceptance::PossiblyAccepted,
            )
        })? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            terminate_process_group(child);
            return Err(ProviderAttemptFailure::after_transport(
                VoxError::new(
                    ErrorCategory::Timeout,
                    "provider.command_timeout",
                    "command provider timed out",
                )
                .with_retryable(true),
                AudioAcceptance::PossiblyAccepted,
            ));
        }
        if cancellation.is_cancelled() {
            terminate_process_group(child);
            return Err(ProviderAttemptFailure::after_transport(
                cancelled_error(),
                AudioAcceptance::PossiblyAccepted,
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn finish_command_output(
    status: std::process::ExitStatus,
    output: BoundedCommandOutput,
) -> Result<String, ProviderAttemptFailure> {
    if output.overflowed {
        return Err(ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.command_output_too_large",
                "command provider output exceeded 1048576 bytes",
            ),
            AudioAcceptance::Accepted,
        ));
    }
    if !status.success() {
        return Err(ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Unavailable,
                "provider.command_exit",
                format!("command exited with {status}"),
            )
            .with_retryable(true),
            AudioAcceptance::Accepted,
        ));
    }
    let text = String::from_utf8(output.bytes).map_err(|error| {
        ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.command_output",
                error.to_string(),
            ),
            AudioAcceptance::Accepted,
        )
    })?;
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err(ProviderAttemptFailure::after_transport(
            VoxError::new(
                ErrorCategory::Protocol,
                "provider.command_empty",
                "command provider returned empty output",
            ),
            AudioAcceptance::Accepted,
        ));
    }
    Ok(text)
}

struct BoundedCommandOutput {
    bytes: Vec<u8>,
    overflowed: bool,
}

fn read_command_output(mut reader: impl Read, limit: usize) -> io::Result<BoundedCommandOutput> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut overflowed = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let retained = limit.saturating_sub(bytes.len()).min(count);
        bytes.extend_from_slice(&buffer[..retained]);
        overflowed |= retained < count;
    }
    Ok(BoundedCommandOutput { bytes, overflowed })
}

fn terminate_process_group(child: &mut std::process::Child) {
    let process_group = format!("-{}", child.id());
    let _result = Command::new("kill")
        .args(["-KILL", "--", &process_group])
        .status();
    let _result = child.kill();
    let _result = child.wait();
}

fn cancelled_error() -> VoxError {
    VoxError::new(
        ErrorCategory::Cancelled,
        "provider.cancelled",
        "recognition was cancelled",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn command_adapter(program: &str, args: Vec<String>, timeout_seconds: u64) -> CommandAdapter {
        CommandAdapter {
            id: ProviderId::new("command").expect("ID"),
            program: program.to_owned(),
            args,
            timeout_seconds,
        }
    }

    #[test]
    fn registry_composes_configured_providers() {
        let config: Config = toml::from_str(
            r#"schema_version = 1
default_profile = "test"
[desktop]
[audio]
[profiles.test]
primary = "mock"
[providers.mock]
kind = "mock"
text = "test"
[providers.local]
kind = "command"
program = "/bin/true"
"#,
        )
        .expect("config");
        let registry = build_provider_registry(&config).expect("registry");
        assert!(registry.contains(&ProviderId::new("mock").expect("mock ID")));
        assert!(registry.contains(&ProviderId::new("local").expect("local ID")));
    }

    #[test]
    fn command_provider_returns_stdout() {
        let adapter = command_adapter(
            "/bin/sh",
            vec!["-c".to_owned(), "printf '本地文本'".to_owned()],
            1,
        );
        let output = adapter
            .recognize(
                RecognitionInput {
                    pcm_path: Path::new("/tmp/audio.wav"),
                    language: "zh",
                },
                &CancellationToken::new(),
            )
            .expect("command output");
        assert_eq!(output.transcript.text, "本地文本");
    }

    #[test]
    fn command_provider_times_out_and_kills_descendants() {
        let marker =
            std::env::temp_dir().join(format!("voxtype-provider-timeout-{}", std::process::id()));
        let _result = std::fs::remove_file(&marker);
        let command = format!("(sleep 2; touch '{}') & wait", marker.to_string_lossy());
        let adapter = command_adapter("/bin/sh", vec!["-c".to_owned(), command], 1);
        let error = adapter
            .recognize(
                RecognitionInput {
                    pcm_path: Path::new("/tmp/audio.wav"),
                    language: "zh",
                },
                &CancellationToken::new(),
            )
            .expect_err("timeout");
        assert_eq!(error.error.code(), "provider.command_timeout");
        thread::sleep(Duration::from_millis(1_500));
        assert!(!marker.exists(), "descendant process survived timeout");
    }

    #[test]
    fn command_provider_cancellation_kills_process_group() {
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            trigger.cancel();
        });
        let adapter = command_adapter("/bin/sleep", vec!["5".to_owned()], 10);
        let started = Instant::now();
        let error = adapter
            .recognize(
                RecognitionInput {
                    pcm_path: Path::new("/tmp/audio.wav"),
                    language: "zh",
                },
                &cancellation,
            )
            .expect_err("cancelled");
        assert_eq!(error.error.category(), ErrorCategory::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(1));
        canceller.join().expect("canceller");
    }

    #[test]
    fn command_provider_output_is_bounded() {
        let adapter = command_adapter(
            "/usr/bin/head",
            vec![
                "-c".to_owned(),
                "1048577".to_owned(),
                "/dev/zero".to_owned(),
            ],
            2,
        );
        let error = adapter
            .recognize(
                RecognitionInput {
                    pcm_path: Path::new("/tmp/audio.wav"),
                    language: "zh",
                },
                &CancellationToken::new(),
            )
            .expect_err("oversized output");
        assert_eq!(error.error.code(), "provider.command_output_too_large");
    }
}
