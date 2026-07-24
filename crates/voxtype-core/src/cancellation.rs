//! Runtime-neutral cooperative cancellation.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Cheap cloneable cancellation flag shared across application and adapter
/// boundaries.
///
/// Cancellation is deliberately a level-triggered state rather than a
/// one-shot notification. Workers may check it before opening a transport,
/// while sending audio, and while waiting for a provider response without
/// depending on a particular async runtime.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_by_clones() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }
}
