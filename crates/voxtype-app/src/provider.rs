//! Provider registry and privacy-aware route execution.

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::path::Path;
use std::sync::Arc;
use voxtype_core::{
    AudioAcceptance, CancellationToken, ErrorCategory, FallbackReason, ProviderAttemptFailure,
    ProviderCapabilities, ProviderId, RoutePlan, VoxError, routing::may_fallback,
};

/// Provider-neutral usage reported authoritatively by one recognition API.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// Final provider output accepted by the application boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTranscript {
    pub text: String,
    pub usage: ProviderUsage,
}

/// Successful provider attempt together with audio-disclosure evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSuccess {
    pub transcript: ProviderTranscript,
    pub transport_started: bool,
    pub audio_acceptance: AudioAcceptance,
}

/// Runtime-neutral recorded-audio recognition input.
#[derive(Clone, Copy, Debug)]
pub struct RecognitionInput<'a> {
    pub pcm_path: &'a Path,
    pub language: &'a str,
}

/// The one runtime contract used by application orchestration.
///
/// Concrete adapters own credentials, transport setup, codecs, and response
/// parsing. They must return conservative audio-acceptance evidence on every
/// failure so the application can enforce replay consent.
pub trait ProviderAdapter: Send + Sync {
    fn id(&self) -> &ProviderId;
    fn capabilities(&self) -> ProviderCapabilities;

    /// Recognizes one recorded PCM input.
    ///
    /// # Errors
    ///
    /// Returns a normalized failure with transport and audio-acceptance
    /// evidence.
    fn recognize(
        &self,
        input: RecognitionInput<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProviderSuccess, ProviderAttemptFailure>;
}

/// Build-time composed provider instances indexed by stable application ID.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    adapters: BTreeMap<ProviderId, Arc<dyn ProviderAdapter>>,
}

impl Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("ids", &self.adapters.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            adapters: BTreeMap::new(),
        }
    }

    /// Registers one composed adapter.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the ID is already registered.
    pub fn register(&mut self, adapter: Arc<dyn ProviderAdapter>) -> Result<(), VoxError> {
        let id = adapter.id().clone();
        if self.adapters.contains_key(&id) {
            return Err(VoxError::new(
                ErrorCategory::Configuration,
                "provider.duplicate_registration",
                format!("provider {id} is already registered"),
            ));
        }
        self.adapters.insert(id, adapter);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: &ProviderId) -> Option<&Arc<dyn ProviderAdapter>> {
        self.adapters.get(id)
    }

    #[must_use]
    pub fn contains(&self, id: &ProviderId) -> bool {
        self.adapters.contains_key(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &ProviderId> {
        self.adapters.keys()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }
}

/// Lifecycle evidence retained for every provider attempted by a route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteAttempt {
    pub provider_id: ProviderId,
    pub transport_started: bool,
    pub audio_acceptance: AudioAcceptance,
    pub error: Option<VoxError>,
}

/// Successful routed recognition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecognitionSuccess {
    pub provider_id: ProviderId,
    pub transcript: ProviderTranscript,
}

/// Complete route result, including failed attempts for health and usage
/// accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecognitionRouteResult {
    pub attempts: Vec<RouteAttempt>,
    pub outcome: Result<RecognitionSuccess, VoxError>,
}

/// Executes providers in route order while enforcing replay consent.
#[must_use]
pub fn run_recognition_route(
    registry: &ProviderRegistry,
    route: &RoutePlan,
    input: RecognitionInput<'_>,
    cancellation: &CancellationToken,
) -> RecognitionRouteResult {
    let mut attempts = Vec::new();
    let mut last_error = None;

    for provider_id in &route.providers {
        if cancellation.is_cancelled() {
            last_error = Some(cancelled_error());
            break;
        }

        let Some(provider) = registry.get(provider_id) else {
            let error = VoxError::new(
                ErrorCategory::Configuration,
                "provider.not_registered",
                format!("provider {provider_id} has no runtime adapter"),
            );
            attempts.push(RouteAttempt {
                provider_id: provider_id.clone(),
                transport_started: false,
                audio_acceptance: AudioAcceptance::NotAccepted,
                error: Some(error.clone()),
            });
            last_error = Some(error);
            break;
        };

        match provider.recognize(input, cancellation) {
            Ok(success) if !success.transcript.text.trim().is_empty() => {
                attempts.push(RouteAttempt {
                    provider_id: provider_id.clone(),
                    transport_started: success.transport_started,
                    audio_acceptance: success.audio_acceptance,
                    error: None,
                });
                return RecognitionRouteResult {
                    attempts,
                    outcome: Ok(RecognitionSuccess {
                        provider_id: provider_id.clone(),
                        transcript: success.transcript,
                    }),
                };
            }
            Ok(success) => {
                let error = VoxError::new(
                    ErrorCategory::Protocol,
                    "provider.empty_transcript",
                    "provider returned an empty final transcript",
                );
                attempts.push(RouteAttempt {
                    provider_id: provider_id.clone(),
                    transport_started: success.transport_started,
                    audio_acceptance: success.audio_acceptance,
                    error: Some(error.clone()),
                });
                last_error = Some(error);
                break;
            }
            Err(failure) => {
                let ProviderAttemptFailure {
                    error,
                    transport_started,
                    audio_acceptance,
                } = failure;
                let retryable = error.is_retryable();
                let cancelled = error.category() == ErrorCategory::Cancelled;
                let fallback_allowed = fallback_reason(error.category())
                    .is_some_and(|reason| may_fallback(reason, audio_acceptance, route.replay));
                attempts.push(RouteAttempt {
                    provider_id: provider_id.clone(),
                    transport_started,
                    audio_acceptance,
                    error: Some(error.clone()),
                });
                last_error = Some(error);
                if cancelled || !retryable || !fallback_allowed {
                    break;
                }
            }
        }
    }

    RecognitionRouteResult {
        attempts,
        outcome: Err(last_error.unwrap_or_else(|| {
            VoxError::new(
                ErrorCategory::Unavailable,
                "provider.no_route",
                "no provider was attempted",
            )
        })),
    }
}

const fn fallback_reason(category: ErrorCategory) -> Option<FallbackReason> {
    match category {
        ErrorCategory::Connection => Some(FallbackReason::Connection),
        ErrorCategory::Timeout => Some(FallbackReason::Timeout),
        ErrorCategory::RateLimited => Some(FallbackReason::RateLimited),
        ErrorCategory::Unavailable => Some(FallbackReason::Unavailable),
        ErrorCategory::InvalidArgument
        | ErrorCategory::InvalidState
        | ErrorCategory::Configuration
        | ErrorCategory::Authentication
        | ErrorCategory::Permission
        | ErrorCategory::Protocol
        | ErrorCategory::Cancelled
        | ErrorCategory::Internal => None,
    }
}

fn cancelled_error() -> VoxError {
    VoxError::new(
        ErrorCategory::Cancelled,
        "provider.cancelled",
        "provider work was cancelled",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use voxtype_core::{AudioFormat, ReplayPolicy, SampleFormat};

    #[derive(Debug)]
    struct FixedAdapter {
        id: ProviderId,
        result: Mutex<Result<ProviderSuccess, ProviderAttemptFailure>>,
    }

    impl ProviderAdapter for FixedAdapter {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                languages: vec!["zh".to_owned()],
                accepted_formats: vec![AudioFormat {
                    sample_rate_hz: 16_000,
                    channels: 1,
                    sample_format: SampleFormat::I16Le,
                }],
                streaming: false,
                partial_results: false,
                provider_vad: false,
            }
        }

        fn recognize(
            &self,
            _input: RecognitionInput<'_>,
            _cancellation: &CancellationToken,
        ) -> Result<ProviderSuccess, ProviderAttemptFailure> {
            self.result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    fn id(value: &str) -> ProviderId {
        ProviderId::new(value).expect("test provider ID")
    }

    fn success(text: &str) -> ProviderSuccess {
        ProviderSuccess {
            transcript: ProviderTranscript {
                text: text.to_owned(),
                usage: ProviderUsage::default(),
            },
            transport_started: false,
            audio_acceptance: AudioAcceptance::NotAccepted,
        }
    }

    fn adapter(
        name: &str,
        result: Result<ProviderSuccess, ProviderAttemptFailure>,
    ) -> Arc<dyn ProviderAdapter> {
        Arc::new(FixedAdapter {
            id: id(name),
            result: Mutex::new(result),
        })
    }

    fn input() -> RecognitionInput<'static> {
        RecognitionInput {
            pcm_path: Path::new("/tmp/test.pcm"),
            language: "zh",
        }
    }

    #[test]
    fn registry_rejects_duplicate_ids() {
        let mut registry = ProviderRegistry::new();
        registry
            .register(adapter("same", Ok(success("one"))))
            .expect("first registration");
        let error = registry
            .register(adapter("same", Ok(success("two"))))
            .expect_err("duplicate must fail");
        assert_eq!(error.code(), "provider.duplicate_registration");
    }

    #[test]
    fn route_falls_back_only_before_audio_acceptance() {
        let retryable = VoxError::new(
            ErrorCategory::Connection,
            "provider.connection",
            "connection failed",
        )
        .with_retryable(true);
        let mut registry = ProviderRegistry::new();
        registry
            .register(adapter(
                "first",
                Err(ProviderAttemptFailure::before_transport(retryable)),
            ))
            .expect("first");
        registry
            .register(adapter("second", Ok(success("fallback"))))
            .expect("second");
        let route = RoutePlan {
            providers: vec![id("first"), id("second")],
            replay: ReplayPolicy::BeforeAudioAccepted,
        };
        let result = run_recognition_route(&registry, &route, input(), &CancellationToken::new());
        assert_eq!(result.attempts.len(), 2);
        assert_eq!(
            result.outcome.expect("fallback succeeds").transcript.text,
            "fallback"
        );
    }

    #[test]
    fn route_does_not_replay_accepted_audio_without_consent() {
        let retryable = VoxError::new(
            ErrorCategory::Timeout,
            "provider.timeout",
            "request timed out",
        )
        .with_retryable(true);
        let mut registry = ProviderRegistry::new();
        registry
            .register(adapter(
                "first",
                Err(ProviderAttemptFailure::after_transport(
                    retryable,
                    AudioAcceptance::Accepted,
                )),
            ))
            .expect("first");
        registry
            .register(adapter("second", Ok(success("must not run"))))
            .expect("second");
        let route = RoutePlan {
            providers: vec![id("first"), id("second")],
            replay: ReplayPolicy::Never,
        };
        let result = run_recognition_route(&registry, &route, input(), &CancellationToken::new());
        assert_eq!(result.attempts.len(), 1);
        assert!(result.outcome.is_err());
    }

    #[test]
    fn pre_cancelled_route_never_invokes_an_adapter() {
        let mut registry = ProviderRegistry::new();
        registry
            .register(adapter("provider", Ok(success("late"))))
            .expect("provider");
        let route = RoutePlan {
            providers: vec![id("provider")],
            replay: ReplayPolicy::Never,
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = run_recognition_route(&registry, &route, input(), &cancellation);
        assert!(result.attempts.is_empty());
        assert_eq!(
            result.outcome.expect_err("cancelled").category(),
            ErrorCategory::Cancelled
        );
    }
}
