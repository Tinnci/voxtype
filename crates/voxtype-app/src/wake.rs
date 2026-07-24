//! Coalescing wake-up channel for the blocking daemon loop.

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

/// Cloneable, non-blocking notification handle.
///
/// The channel has capacity one, so bursts coalesce without allowing an
/// unbounded queue or blocking a D-Bus/provider worker.
#[derive(Clone, Debug, Default)]
pub struct WakeHandle {
    sender: Option<SyncSender<()>>,
}

impl WakeHandle {
    #[must_use]
    pub const fn disabled() -> Self {
        Self { sender: None }
    }

    pub fn notify(&self) {
        if let Some(sender) = &self.sender {
            let _result = sender.try_send(());
        }
    }
}

/// Creates a capacity-one wake channel.
#[must_use]
pub fn wake_channel() -> (WakeHandle, Receiver<()>) {
    let (sender, receiver) = sync_channel(1);
    (
        WakeHandle {
            sender: Some(sender),
        },
        receiver,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_wakes_coalesce_without_blocking() {
        let (wake, receiver) = wake_channel();
        wake.notify();
        wake.notify();
        assert_eq!(receiver.try_recv(), Ok(()));
        assert!(receiver.try_recv().is_err());
    }
}
