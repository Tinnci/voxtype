//! Single-writer application state and provider runtime accounting.

use crate::{ProviderRegistry, ProviderUsage, RecognitionRouteResult, WakeHandle};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};
use voxtype_core::{
    Command, CommandEffect, ErrorCategory, ProviderHealth, ProviderId, ProviderRouter, RoutePlan,
    RoutingPolicy, SessionId, SessionMachine, SessionState, VoxError, routing::OrderedRouter,
};

pub const MAX_PENDING_EVENTS: usize = 256;
pub const MAX_SESSION_RESULTS: usize = 32;
pub const PROVIDER_VERIFICATION_TTL: Duration = Duration::from_secs(15 * 60);

/// Transcript-free lifecycle event emitted to desktop and IPC adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppEvent {
    StateChanged {
        state: String,
        session: String,
    },
    SessionFinished {
        session: String,
        outcome: String,
        error_code: String,
        backend: String,
        char_count: u64,
    },
}

/// Bounded transcript-free terminal result retained for polling clients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSessionResult {
    pub session: String,
    pub outcome: String,
    pub error_code: String,
    pub backend: String,
    pub char_count: u64,
}

#[derive(Clone, Debug, Default)]
struct ProviderHealthState {
    consecutive_failures: u32,
    blocked_until: Option<Instant>,
    last_success_at: Option<Instant>,
    last_failure_at: Option<Instant>,
    last_error_category: Option<ErrorCategory>,
    last_error_code: Option<&'static str>,
}

/// Time-relative provider health exposed to outer presentation adapters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderHealthSnapshot {
    pub route_available: bool,
    pub verified: bool,
    pub verified_age_seconds: Option<u64>,
    pub verification_ttl_seconds: u64,
    pub consecutive_failures: u32,
    pub retry_after_seconds: Option<u64>,
    pub last_failure_age_seconds: Option<u64>,
    pub last_error_category: Option<ErrorCategory>,
    pub last_error_code: Option<&'static str>,
}

/// Session-local usage counters owned by the application layer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderUsageSnapshot {
    pub attempts: u64,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub audio_millis: u64,
    pub token_reports: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub reported_tokens: u64,
}

/// The only owner of session state, provider runtime state, and lifecycle
/// events.
#[derive(Debug)]
pub struct AppController {
    machine: SessionMachine,
    events: VecDeque<AppEvent>,
    session_results: VecDeque<TerminalSessionResult>,
    registry: ProviderRegistry,
    provider_health: BTreeMap<ProviderId, ProviderHealthState>,
    provider_usage: BTreeMap<ProviderId, ProviderUsageSnapshot>,
    wake: WakeHandle,
}

impl AppController {
    #[must_use]
    pub fn new(registry: ProviderRegistry) -> Self {
        Self::with_wake(registry, WakeHandle::disabled())
    }

    #[must_use]
    pub fn with_wake(registry: ProviderRegistry, wake: WakeHandle) -> Self {
        Self {
            machine: SessionMachine::default(),
            events: VecDeque::from([AppEvent::StateChanged {
                state: "idle".to_owned(),
                session: String::new(),
            }]),
            session_results: VecDeque::with_capacity(MAX_SESSION_RESULTS),
            registry,
            provider_health: BTreeMap::new(),
            provider_usage: BTreeMap::new(),
            wake,
        }
    }

    #[must_use]
    pub const fn state(&self) -> &SessionState {
        self.machine.state()
    }

    #[must_use]
    pub fn state_snapshot(&self) -> (String, String) {
        (
            self.machine.state().name().to_owned(),
            self.machine
                .state()
                .session()
                .map_or_else(String::new, ToString::to_string),
        )
    }

    /// Applies one serialized domain command and publishes its complete
    /// lifecycle sequence.
    ///
    /// # Errors
    ///
    /// Returns a domain transition error or bounded-event backpressure.
    pub fn apply(&mut self, command: Command) -> Result<CommandEffect, VoxError> {
        if self.events.len() > MAX_PENDING_EVENTS.saturating_sub(2) {
            return Err(VoxError::new(
                ErrorCategory::Unavailable,
                "app.event_backpressure",
                "application event queue is temporarily full",
            )
            .with_retryable(true));
        }
        let terminal = match &command {
            Command::Cancel { session } => Some((session.clone(), "cancelled", "")),
            Command::NoSpeech { session } => Some((session.clone(), "no-speech", "")),
            Command::Fail { session, error } => Some((session.clone(), "failed", error.code())),
            _ => None,
        };
        let effect = self.machine.apply(command)?;
        self.queue_current_state();
        if let Some((session, outcome, error_code)) = terminal {
            self.queue_session_finished(&session, outcome, error_code, "", 0);
        }
        self.wake.notify();
        Ok(effect)
    }

    /// Records the metadata-only terminal result after successful insertion.
    ///
    /// # Errors
    ///
    /// Returns bounded-event backpressure without altering the result cache.
    pub fn finish_session(
        &mut self,
        session: &SessionId,
        outcome: &str,
        error_code: &str,
        backend: &str,
        char_count: u64,
    ) -> Result<(), VoxError> {
        if self.events.len() == MAX_PENDING_EVENTS {
            return Err(VoxError::new(
                ErrorCategory::Unavailable,
                "app.event_backpressure",
                "application event queue is temporarily full",
            )
            .with_retryable(true));
        }
        self.queue_session_finished(session, outcome, error_code, backend, char_count);
        self.wake.notify();
        Ok(())
    }

    #[must_use]
    pub fn session_result(&self, session: &str) -> Option<TerminalSessionResult> {
        self.session_results
            .iter()
            .rev()
            .find(|result| result.session == session)
            .cloned()
    }

    #[must_use]
    pub fn drain_events(&mut self) -> Vec<AppEvent> {
        self.events.drain(..).collect()
    }

    #[must_use]
    pub fn registry(&self) -> ProviderRegistry {
        self.registry.clone()
    }

    pub fn replace_registry(&mut self, registry: ProviderRegistry) {
        self.registry = registry;
        self.provider_health.clear();
    }

    #[must_use]
    pub fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }

    /// Plans a route from current cooldown state and registered adapters.
    ///
    /// # Errors
    ///
    /// Returns unavailable when every configured adapter is absent or cooling
    /// down.
    pub fn plan_route(&self, policy: &RoutingPolicy, now: Instant) -> Result<RoutePlan, VoxError> {
        let health = std::iter::once(&policy.primary)
            .chain(policy.fallbacks.iter())
            .map(|id| {
                let registered = self.registry.contains(id);
                let available = registered
                    && self
                        .provider_health
                        .get(id)
                        .is_none_or(|state| state.is_available_at(now));
                (
                    id.clone(),
                    ProviderHealth {
                        available,
                        consecutive_failures: self
                            .provider_health
                            .get(id)
                            .map_or(0, |state| state.consecutive_failures),
                    },
                )
            })
            .collect();
        OrderedRouter.plan(policy, &health)
    }

    /// Applies one completed route to health and usage accounting exactly once.
    pub fn record_route_result(
        &mut self,
        result: &RecognitionRouteResult,
        audio_millis: u64,
        now: Instant,
    ) {
        for attempt in &result.attempts {
            let usage = self
                .provider_usage
                .entry(attempt.provider_id.clone())
                .or_default();
            usage.attempts = usage.attempts.saturating_add(1);
            if attempt.transport_started {
                usage.requests = usage.requests.saturating_add(1);
            }
            if attempt.audio_acceptance.may_have_left_process() {
                usage.audio_millis = usage.audio_millis.saturating_add(audio_millis);
            }
            if let Some(error) = &attempt.error {
                if error.category() == ErrorCategory::Cancelled {
                    continue;
                }
                usage.failures = usage.failures.saturating_add(1);
                self.provider_health
                    .entry(attempt.provider_id.clone())
                    .or_default()
                    .record_failure_at(now, error);
            }
        }

        if let Ok(success) = &result.outcome {
            self.provider_health
                .entry(success.provider_id.clone())
                .or_default()
                .record_success_at(now);
            let usage = self
                .provider_usage
                .entry(success.provider_id.clone())
                .or_default();
            usage.record_success(success.transcript.usage);
        }
    }

    #[must_use]
    pub fn provider_health_snapshot(
        &self,
        provider_id: &ProviderId,
        now: Instant,
    ) -> ProviderHealthSnapshot {
        let state = self.provider_health.get(provider_id);
        let age = |instant: Option<Instant>| {
            instant.map(|value| now.saturating_duration_since(value).as_secs())
        };
        ProviderHealthSnapshot {
            route_available: self.registry.contains(provider_id)
                && state.is_none_or(|health| health.is_available_at(now)),
            verified: state.is_some_and(|health| health.verified_at(now)),
            verified_age_seconds: state.and_then(|health| age(health.last_success_at)),
            verification_ttl_seconds: PROVIDER_VERIFICATION_TTL.as_secs(),
            consecutive_failures: state.map_or(0, |health| health.consecutive_failures),
            retry_after_seconds: state
                .and_then(|health| health.blocked_until)
                .and_then(|deadline| deadline.checked_duration_since(now))
                .map(|duration| duration.as_secs()),
            last_failure_age_seconds: state.and_then(|health| age(health.last_failure_at)),
            last_error_category: state.and_then(|health| health.last_error_category),
            last_error_code: state.and_then(|health| health.last_error_code),
        }
    }

    #[must_use]
    pub fn provider_usage_snapshot(&self, provider_id: &ProviderId) -> ProviderUsageSnapshot {
        self.provider_usage
            .get(provider_id)
            .cloned()
            .unwrap_or_default()
    }

    fn queue_current_state(&mut self) {
        self.events.push_back(AppEvent::StateChanged {
            state: self.machine.state().name().to_owned(),
            session: self
                .machine
                .state()
                .session()
                .map_or_else(String::new, ToString::to_string),
        });
    }

    fn queue_session_finished(
        &mut self,
        session: &SessionId,
        outcome: &str,
        error_code: &str,
        backend: &str,
        char_count: u64,
    ) {
        if let Some(position) = self
            .session_results
            .iter()
            .position(|result| result.session == session.as_str())
        {
            self.session_results.remove(position);
        }
        if self.session_results.len() == MAX_SESSION_RESULTS {
            self.session_results.pop_front();
        }
        self.session_results.push_back(TerminalSessionResult {
            session: session.to_string(),
            outcome: outcome.to_owned(),
            error_code: error_code.to_owned(),
            backend: backend.to_owned(),
            char_count,
        });
        self.events.push_back(AppEvent::SessionFinished {
            session: session.to_string(),
            outcome: outcome.to_owned(),
            error_code: error_code.to_owned(),
            backend: backend.to_owned(),
            char_count,
        });
    }
}

impl ProviderHealthState {
    fn is_available_at(&self, now: Instant) -> bool {
        self.blocked_until.is_none_or(|deadline| now >= deadline)
    }

    fn record_success_at(&mut self, now: Instant) {
        self.last_success_at = Some(now);
        self.consecutive_failures = 0;
        self.blocked_until = None;
    }

    fn record_failure_at(&mut self, now: Instant, error: &VoxError) {
        self.last_failure_at = Some(now);
        self.last_error_category = Some(error.category());
        self.last_error_code = Some(error.code());
        if error.is_retryable() {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if self.consecutive_failures >= 3 {
                self.blocked_until = Some(now + Duration::from_secs(60));
            }
        }
    }

    fn verified_at(&self, now: Instant) -> bool {
        self.last_success_at.is_some_and(|success| {
            now.saturating_duration_since(success) <= PROVIDER_VERIFICATION_TTL
        })
    }
}

impl ProviderUsageSnapshot {
    fn record_success(&mut self, usage: ProviderUsage) {
        self.successes = self.successes.saturating_add(1);
        if usage.input_tokens.is_none()
            && usage.output_tokens.is_none()
            && usage.total_tokens.is_none()
        {
            return;
        }
        self.token_reports = self.token_reports.saturating_add(1);
        self.input_tokens = self
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or(0));
        self.total_tokens = self
            .total_tokens
            .saturating_add(usage.total_tokens.unwrap_or(0));
        let reported = usage.total_tokens.unwrap_or_else(|| {
            usage
                .input_tokens
                .unwrap_or(0)
                .saturating_add(usage.output_tokens.unwrap_or(0))
        });
        self.reported_tokens = self.reported_tokens.saturating_add(reported);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderSuccess, ProviderTranscript, RecognitionSuccess, RouteAttempt};
    use voxtype_core::{AudioAcceptance, ReplayPolicy, StartRequest, TriggerMode};

    fn id(value: &str) -> ProviderId {
        ProviderId::new(value).expect("provider ID")
    }

    fn controller() -> AppController {
        AppController::new(ProviderRegistry::new())
    }

    #[test]
    fn emits_every_happy_path_state_in_order() {
        let mut app = controller();
        let effect = app
            .apply(Command::Start(StartRequest {
                mode: TriggerMode::Toggle,
                profile: None,
            }))
            .expect("start");
        let CommandEffect::BeginCapture { session, .. } = effect else {
            panic!("expected capture effect");
        };
        app.apply(Command::CaptureReady {
            session: session.clone(),
        })
        .expect("capture ready");
        app.apply(Command::Stop {
            session: session.clone(),
        })
        .expect("stop");
        app.apply(Command::TranscriptReady {
            session: session.clone(),
            text: "hello".to_owned(),
        })
        .expect("transcript");
        app.apply(Command::InsertionComplete {
            session: session.clone(),
        })
        .expect("insertion");
        app.finish_session(&session, "completed", "", "fcitx", 5)
            .expect("finish");

        let events = app.drain_events();
        let states = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::StateChanged { state, .. } => Some(state.as_str()),
                AppEvent::SessionFinished { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            [
                "idle",
                "preparing",
                "listening",
                "finalizing",
                "inserting",
                "completed"
            ]
        );
    }

    #[test]
    fn terminal_result_cache_is_bounded_and_idempotent() {
        let mut app = controller();
        for counter in 0..=MAX_SESSION_RESULTS {
            let session = SessionId::from_counter(u64::try_from(counter).expect("counter"));
            app.finish_session(&session, "cancelled", "", "", 0)
                .expect("finish");
        }
        assert_eq!(app.session_results.len(), MAX_SESSION_RESULTS);
        assert!(
            app.session_result(SessionId::from_counter(0).as_str())
                .is_none()
        );
        let latest = SessionId::from_counter(u64::try_from(MAX_SESSION_RESULTS).expect("counter"));
        app.finish_session(&latest, "completed", "", "copy-only", 3)
            .expect("update");
        assert_eq!(
            app.session_result(latest.as_str()).expect("latest").outcome,
            "completed"
        );
    }

    #[test]
    fn provider_health_blocks_after_three_retryable_failures() {
        let mut app = controller();
        let provider_id = id("provider");
        let error = VoxError::new(
            ErrorCategory::Connection,
            "provider.connection",
            "connection failed",
        )
        .with_retryable(true);
        let result = RecognitionRouteResult {
            attempts: vec![RouteAttempt {
                provider_id: provider_id.clone(),
                transport_started: false,
                audio_acceptance: AudioAcceptance::NotAccepted,
                error: Some(error),
            }],
            outcome: Err(VoxError::new(
                ErrorCategory::Connection,
                "provider.connection",
                "connection failed",
            )),
        };
        let now = Instant::now();
        app.record_route_result(&result, 1_000, now);
        app.record_route_result(&result, 1_000, now);
        assert_eq!(
            app.provider_health_snapshot(&provider_id, now)
                .consecutive_failures,
            2
        );
        app.record_route_result(&result, 1_000, now);
        let health = app.provider_health_snapshot(&provider_id, now + Duration::from_secs(59));
        assert!(!health.route_available);
        assert_eq!(health.retry_after_seconds, Some(1));
    }

    #[test]
    fn usage_counts_only_authoritative_token_reports() {
        let mut app = controller();
        let provider_id = id("provider");
        let result = RecognitionRouteResult {
            attempts: vec![RouteAttempt {
                provider_id: provider_id.clone(),
                transport_started: true,
                audio_acceptance: AudioAcceptance::Accepted,
                error: None,
            }],
            outcome: Ok(RecognitionSuccess {
                provider_id: provider_id.clone(),
                transcript: ProviderTranscript {
                    text: "text".to_owned(),
                    usage: ProviderUsage {
                        input_tokens: Some(12),
                        output_tokens: Some(3),
                        total_tokens: Some(15),
                    },
                },
            }),
        };
        app.record_route_result(&result, 750, Instant::now());
        let usage = app.provider_usage_snapshot(&provider_id);
        assert_eq!(usage.attempts, 1);
        assert_eq!(usage.requests, 1);
        assert_eq!(usage.successes, 1);
        assert_eq!(usage.audio_millis, 750);
        assert_eq!(usage.reported_tokens, 15);
    }

    #[test]
    fn cancellation_queues_state_before_terminal_metadata() {
        let mut app = controller();
        let effect = app
            .apply(Command::Start(StartRequest {
                mode: TriggerMode::Toggle,
                profile: None,
            }))
            .expect("start");
        let CommandEffect::BeginCapture { session, .. } = effect else {
            panic!("capture");
        };
        app.apply(Command::Cancel { session }).expect("cancel");
        let events = app.drain_events();
        assert!(matches!(
            events.as_slice(),
            [
                AppEvent::StateChanged { state: idle, .. },
                AppEvent::StateChanged {
                    state: preparing,
                    ..
                },
                AppEvent::StateChanged {
                    state: cancelled,
                    ..
                },
                AppEvent::SessionFinished { outcome, .. }
            ] if idle == "idle"
                && preparing == "preparing"
                && cancelled == "cancelled"
                && outcome == "cancelled"
        ));
    }

    #[test]
    fn provider_success_resets_retryable_failure_count() {
        let mut health = ProviderHealthState::default();
        let now = Instant::now();
        let error = VoxError::new(ErrorCategory::Timeout, "provider.timeout", "timeout")
            .with_retryable(true);
        health.record_failure_at(now, &error);
        health.record_success_at(now + Duration::from_secs(1));
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.verified_at(now + Duration::from_secs(1)));
    }

    #[test]
    fn provider_usage_without_tokens_stays_unknown() {
        let mut usage = ProviderUsageSnapshot::default();
        usage.record_success(ProviderUsage::default());
        assert_eq!(usage.successes, 1);
        assert_eq!(usage.token_reports, 0);
        assert_eq!(usage.reported_tokens, 0);
    }

    #[test]
    fn successful_attempt_type_remains_provider_neutral() {
        let output = ProviderSuccess {
            transcript: ProviderTranscript {
                text: "hello".to_owned(),
                usage: ProviderUsage::default(),
            },
            transport_started: true,
            audio_acceptance: AudioAcceptance::Accepted,
        };
        assert_eq!(output.transcript.text, "hello");
    }

    #[test]
    fn route_policy_retains_replay_setting() {
        let policy = RoutingPolicy {
            primary: id("primary"),
            fallbacks: vec![id("fallback")],
            replay: ReplayPolicy::BufferedWithConsent,
        };
        assert_eq!(policy.replay, ReplayPolicy::BufferedWithConsent);
    }
}
