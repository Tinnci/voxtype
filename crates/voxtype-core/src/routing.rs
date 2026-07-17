//! Provider selection and privacy-aware fallback policy.

use crate::{AudioAcceptance, ErrorCategory, ProviderId, VoxError};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReplayPolicy {
    #[default]
    Never,
    BeforeAudioAccepted,
    BufferedWithConsent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackReason {
    Connection,
    Timeout,
    RateLimited,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderHealth {
    pub available: bool,
    pub consecutive_failures: u32,
}

impl Default for ProviderHealth {
    fn default() -> Self {
        Self {
            available: true,
            consecutive_failures: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutingPolicy {
    pub primary: ProviderId,
    pub fallbacks: Vec<ProviderId>,
    pub replay: ReplayPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutePlan {
    pub providers: Vec<ProviderId>,
    pub replay: ReplayPolicy,
}

pub trait ProviderRouter {
    /// Builds an ordered route from configured policy and current health.
    ///
    /// # Errors
    ///
    /// Returns an unavailable error when no configured provider is healthy.
    fn plan(
        &self,
        policy: &RoutingPolicy,
        health: &BTreeMap<ProviderId, ProviderHealth>,
    ) -> Result<RoutePlan, VoxError>;
}

#[derive(Debug, Default)]
pub struct OrderedRouter;

impl ProviderRouter for OrderedRouter {
    fn plan(
        &self,
        policy: &RoutingPolicy,
        health: &BTreeMap<ProviderId, ProviderHealth>,
    ) -> Result<RoutePlan, VoxError> {
        let providers = std::iter::once(&policy.primary)
            .chain(policy.fallbacks.iter())
            .filter(|id| health.get(*id).is_none_or(|status| status.available))
            .cloned()
            .collect::<Vec<_>>();

        if providers.is_empty() {
            return Err(VoxError::new(
                ErrorCategory::Unavailable,
                "routing.no_provider",
                "no configured provider is currently available",
            )
            .with_retryable(true));
        }

        Ok(RoutePlan {
            providers,
            replay: policy.replay,
        })
    }
}

#[must_use]
pub const fn may_fallback(
    reason: FallbackReason,
    audio_acceptance: AudioAcceptance,
    replay: ReplayPolicy,
) -> bool {
    let transient = matches!(
        reason,
        FallbackReason::Connection
            | FallbackReason::Timeout
            | FallbackReason::RateLimited
            | FallbackReason::Unavailable
    );
    transient
        && (matches!(audio_acceptance, AudioAcceptance::NotAccepted)
            || matches!(replay, ReplayPolicy::BufferedWithConsent))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> ProviderId {
        ProviderId::new(value).expect("test provider ID must be valid")
    }

    #[test]
    fn skips_unhealthy_provider_in_order() {
        let policy = RoutingPolicy {
            primary: id("primary"),
            fallbacks: vec![id("backup")],
            replay: ReplayPolicy::Never,
        };
        let health = BTreeMap::from([(
            id("primary"),
            ProviderHealth {
                available: false,
                consecutive_failures: 3,
            },
        )]);

        let plan = OrderedRouter
            .plan(&policy, &health)
            .expect("backup is healthy");
        assert_eq!(plan.providers, vec![id("backup")]);
    }

    #[test]
    fn blocks_cloud_replay_without_consent() {
        assert!(!may_fallback(
            FallbackReason::Unavailable,
            AudioAcceptance::Accepted,
            ReplayPolicy::Never
        ));
        assert!(may_fallback(
            FallbackReason::Unavailable,
            AudioAcceptance::Accepted,
            ReplayPolicy::BufferedWithConsent
        ));
        assert!(!may_fallback(
            FallbackReason::Connection,
            AudioAcceptance::PossiblyAccepted,
            ReplayPolicy::BeforeAudioAccepted
        ));
        assert!(may_fallback(
            FallbackReason::Connection,
            AudioAcceptance::NotAccepted,
            ReplayPolicy::BeforeAudioAccepted
        ));
    }
}
