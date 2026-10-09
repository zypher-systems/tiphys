//! Stopping a turn that is under way.
//!
//! The owner presses a key, sends `/stop`, or closes the app, and the turn
//! has to end now: in the middle of a reply, or of a tool that is running.
//! Everything the agent waits on is raced against [`Cancel::cancelled`].

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A flag one side sets and the other waits on.
#[derive(Debug, Default)]
pub struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    /// Asks for the turn to stop.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Clears the flag, for the next turn.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Returns once a stop has been asked for, at once if it already was.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register before looking at the flag, so a cancel that lands
            // between the look and the wait is not missed.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn a_cancel_wakes_whoever_is_waiting_and_stays_set() {
        let cancel = Arc::new(Cancel::default());
        let waiter = {
            let cancel = cancel.clone();
            tokio::spawn(async move { cancel.cancelled().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
        // Already cancelled: no waiting.
        cancel.cancelled().await;
        assert!(cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn a_reset_makes_it_wait_again() {
        let cancel = Cancel::default();
        cancel.cancel();
        cancel.reset();
        assert!(!cancel.is_cancelled());
        let waited = tokio::time::timeout(Duration::from_secs(1), cancel.cancelled()).await;
        assert!(waited.is_err());
    }
}
