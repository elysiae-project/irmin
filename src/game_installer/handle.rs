use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::error::{SophonError, SophonResult};
use crate::SophonProgress;

const STATE_RUNNING: u8 = 0;
const STATE_PAUSED: u8 = 1;
/// Terminal cancelled state; value 3 avoids collision with future states.
const STATE_CANCELLED: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Snapshot of a download's cooperative pause and cancellation state.
pub enum ControlState {
    Running,
    Paused,
    Cancelled,
}

#[derive(Clone)]
/// Cloneable control handle shared with an in-progress operation.
/// Pausing is cooperative; cancellation is permanent for this handle.
pub struct DownloadHandle {
    state: Arc<AtomicU8>,
    cancel_token: CancellationToken,
    pause_notify: Arc<Notify>,
}

impl DownloadHandle {
    /// Create an active, unpaused control handle.
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(STATE_RUNNING)),
            cancel_token: CancellationToken::new(),
            pause_notify: Arc::new(Notify::new()),
        }
    }

    /// Request a pause; workers stop at their next pause-aware checkpoint.
    pub fn pause(&self) {
        // compare_exchange avoids racing with cancel; cancellation is terminal.
        while let Err(current) = self.state.compare_exchange(
            STATE_RUNNING,
            STATE_PAUSED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // Abort if state changed (cancelled or already paused).
            if current != STATE_RUNNING {
                return;
            }
        }
    }

    /// Clear the pause request and wake workers waiting in `wait_if_paused`.
    pub fn resume(&self) {
        // Cancellation is terminal.
        if self.state.load(Ordering::Acquire) == STATE_CANCELLED {
            return;
        }
        // Transition PAUSED→RUNNING and wake waiters only on actual transition.
        if self
            .state
            .compare_exchange(
                STATE_PAUSED,
                STATE_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.pause_notify.notify_waiters();
        }
    }

    /// Permanently request cancellation and wake cancellation-aware workers.
    pub fn cancel(&self) {
        self.state.store(STATE_CANCELLED, Ordering::Release);
        self.cancel_token.cancel();
        self.pause_notify.notify_waiters();
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_CANCELLED
    }

    /// Whether a pause request is currently active.
    pub fn is_paused(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_PAUSED
    }

    /// Future that resolves when this handle is cancelled.
    pub fn cancelled_future(&self) -> tokio_util::sync::WaitForCancellationFuture<'_> {
        self.cancel_token.cancelled()
    }

    /// Read the current control state.
    pub fn get_state(&self) -> ControlState {
        match self.state.load(Ordering::Acquire) {
            STATE_RUNNING => ControlState::Running,
            STATE_PAUSED => ControlState::Paused,
            STATE_CANCELLED => ControlState::Cancelled,
            raw => {
                log::error!("DownloadHandle in invalid state: {raw}");
                ControlState::Cancelled
            }
        }
    }

    /// Wait until resumed, or return early if cancellation is requested.
    /// Wait while paused, returning `Err(())` if cancellation occurs first.
    pub async fn wait_if_paused(
        &self,
        updater: &(impl Fn(SophonProgress) + Send + Sync + ?Sized),
        downloaded_bytes: u64,
        total_bytes: u64,
    ) -> SophonResult<()> {
        loop {
            // Capture broadcast notifications before reading state or invoking
            // the callback: either can race with resume/cancel. notify_waiters
            // also wakes an already-created future that has not been polled.
            let notified = self.pause_notify.notified();
            match self.get_state() {
                ControlState::Running => return Ok(()),
                ControlState::Cancelled => return Err(SophonError::Cancelled),
                ControlState::Paused => {
                    updater(SophonProgress::Paused {
                        downloaded_bytes,
                        total_bytes,
                    });
                    notified.await;
                }
            }
        }
    }
}

impl Default for DownloadHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_new_is_running() {
        let handle = DownloadHandle::new();
        assert!(!handle.is_cancelled());
    }

    #[test]
    fn handle_cancel() {
        let handle = DownloadHandle::new();
        handle.cancel();
        assert!(handle.is_cancelled());
    }

    #[test]
    fn handle_pause_resume() {
        let handle = DownloadHandle::new();
        assert_eq!(handle.get_state(), ControlState::Running);

        handle.pause();
        assert_eq!(handle.get_state(), ControlState::Paused);

        handle.resume();
        assert_eq!(handle.get_state(), ControlState::Running);
    }

    #[test]
    fn handle_is_cancelled_after_cancel() {
        let handle = DownloadHandle::new();
        assert!(!handle.is_cancelled());
        handle.cancel();
        assert!(handle.is_cancelled());
    }

    #[tokio::test]
    async fn handle_resume_notifies_waiters() {
        let handle = DownloadHandle::new();
        handle.pause();
        let updater = |_progress: crate::SophonProgress| {};
        let h = handle.clone();
        let result = tokio::spawn(async move { h.wait_if_paused(&updater, 0, 100).await });
        tokio::task::yield_now().await;
        handle.resume();
        let state = result.await.unwrap();
        assert!(state.is_ok());
    }

    #[tokio::test]
    async fn handle_wait_if_paused_returns_cancelled() {
        let handle = DownloadHandle::new();
        handle.pause();
        let updater = |_progress: crate::SophonProgress| {};
        let h = handle.clone();
        let result = tokio::spawn(async move { h.wait_if_paused(&updater, 0, 100).await });
        tokio::task::yield_now().await;
        handle.cancel();
        let state = result.await.unwrap();
        assert!(state.is_err());
        assert!(matches!(state.unwrap_err(), super::SophonError::Cancelled));
    }

    #[tokio::test]
    async fn handle_resume_from_paused_callback() {
        let handle = DownloadHandle::new();
        handle.pause();
        let updater = |_| handle.resume();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.wait_if_paused(&updater, 0, 100),
        )
        .await
        .expect("resume from the callback must not lose its notification");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn handle_cancel_from_paused_callback() {
        let handle = DownloadHandle::new();
        handle.pause();
        let updater = |_| handle.cancel();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.wait_if_paused(&updater, 0, 100),
        )
        .await
        .expect("cancellation from the callback must not lose its notification");
        assert!(matches!(result, Err(SophonError::Cancelled)));
    }

    #[tokio::test]
    async fn handle_resume_then_repause_rechecks_state() {
        let handle = DownloadHandle::new();
        handle.pause();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let updater = |_| {
            handle.resume();
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                handle.pause();
            }
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.wait_if_paused(&updater, 0, 100),
        )
        .await
        .expect("a second pause must be checked after waking")
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn handle_broadcast_wakes_all_waiters_repeatedly() {
        let handle = DownloadHandle::new();
        let updater = |_| {};
        for _ in 0..20 {
            handle.pause();
            let mut waiters: Vec<_> = (0..16)
                .map(|_| Box::pin(handle.wait_if_paused(&updater, 0, 100)))
                .collect();
            for waiter in &mut waiters {
                assert!(futures_util::poll!(waiter.as_mut()).is_pending());
            }
            handle.resume();
            let results = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                futures_util::future::join_all(waiters),
            )
            .await
            .expect("resume must broadcast to every paused worker");
            assert!(results.iter().all(Result::is_ok));
        }
        handle.cancel();
        handle.resume();
        handle.pause();
        assert!(matches!(
            handle.wait_if_paused(&updater, 0, 100).await,
            Err(SophonError::Cancelled)
        ));
    }

    #[test]
    fn handle_multiple_pause_calls() {
        let handle = DownloadHandle::new();
        handle.pause();
        assert_eq!(handle.get_state(), ControlState::Paused);
        handle.pause();
        assert_eq!(handle.get_state(), ControlState::Paused);
        handle.pause();
        assert_eq!(handle.get_state(), ControlState::Paused);
        handle.resume();
        assert_eq!(handle.get_state(), ControlState::Running);
    }

    #[test]
    fn handle_resume_after_cancel_is_noop() {
        let handle = DownloadHandle::new();
        handle.cancel();
        handle.resume();
        assert_eq!(handle.get_state(), ControlState::Cancelled);
        assert!(handle.is_cancelled());
    }

    #[test]
    fn handle_pause_after_cancel_is_noop() {
        let handle = DownloadHandle::new();
        handle.cancel();
        handle.pause();
        assert_eq!(handle.get_state(), ControlState::Cancelled);
        assert!(handle.is_cancelled());
    }

    #[test]
    fn handle_resume_without_prior_pause_is_noop() {
        let handle = DownloadHandle::new();
        // Resuming a running handle must not change state or notify.
        handle.resume();
        assert_eq!(handle.get_state(), ControlState::Running);
    }

    #[tokio::test]
    async fn handle_concurrent_pause_cancel_idempotent() {
        for _ in 0..50 {
            let handle = DownloadHandle::new();
            let h = handle.clone();
            tokio::spawn(async move {
                h.pause();
            });
            handle.cancel();
            tokio::task::yield_now().await;
            // Cancel wins over pause.
            assert!(handle.is_cancelled());
        }
    }
}
