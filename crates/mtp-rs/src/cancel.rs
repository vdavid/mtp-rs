//! Cooperative cancellation for long-running MTP operations.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation handle for long-running MTP operations.
///
/// Cheap to clone (`Arc`-backed). `Send + Sync`. Once cancelled, the token
/// stays cancelled (no reset). Construct a fresh `CancelToken` per
/// logical operation.
///
/// A cheap-to-clone handle that callers pass into operations
/// like [`Storage::list_objects`](crate::mtp::Storage::list_objects) so they can
/// be aborted from another task. The operation checks the token between USB
/// roundtrips; when set, it returns [`Error::Cancelled`](crate::Error::Cancelled)
/// within one roundtrip's worth of latency instead of running to completion.
///
/// # When to use this
///
/// MTP list and recursive delete on large folders consist of many small
/// per-object USB transactions (`GetObjectInfo` per handle). A 1k-photo
/// `/DCIM/Camera` listing on Android can take 15+ seconds over USB 2.0. Without
/// cooperative cancel, a user-initiated cancel only stops issuing _new_
/// transactions after the current bulk call's _entire_ for-loop completes. By
/// then the device may already be wedged from a follow-up op.
///
/// `CancelToken` gives every per-handle iteration a fast way to bail.
///
/// # Mid-data-phase cancel
///
/// For streaming downloads, see
/// [`FileDownload::cancel`](crate::mtp::FileDownload::cancel); that path uses
/// the USB Still Image Class (SIC) class-cancel mechanism to abort a long
/// bulk-IN transfer mid-stream. Per-handle list/delete operations don't need
/// that: each `GetObjectInfo` / `DeleteObject` USB roundtrip completes in
/// milliseconds, so checking the token between roundtrips is sufficient and
/// safe (no half-finished transfer to drain).
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::{CancelToken, mtp::MtpDevice};
///
/// # async fn example() -> Result<(), mtp_rs::Error> {
/// let device = MtpDevice::open_first().await?;
/// let storages = device.storages().await?;
/// let storage = &storages[0];
///
/// let cancel = CancelToken::new();
///
/// // Fire the cancel from another task after 500 ms.
/// let cancel_for_task = cancel.clone();
/// tokio::spawn(async move {
///     tokio::time::sleep(std::time::Duration::from_millis(500)).await;
///     cancel_for_task.cancel();
/// });
///
/// match storage.list_objects_with_cancel(None, Some(&cancel)).await {
///     Ok(objects) => println!("Listed {} objects", objects.len()),
///     Err(mtp_rs::Error::Cancelled) => println!("Cancelled mid-listing"),
///     Err(e) => return Err(e),
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// Creates a fresh, not-yet-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps an externally-owned `Arc<AtomicBool>`.
    ///
    /// Use this when the caller already has its own cancellation flag (for
    /// example, threaded through several layers from a host application) and
    /// wants the same atomic to drive an mtp-rs operation, with no second
    /// polling task. The token shares ownership of the inner atomic, so
    /// flipping the original flag flips the token (and vice versa).
    #[must_use]
    pub fn from_arc(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }

    /// Marks the token as cancelled. Subsequent [`is_cancelled`] calls return
    /// `true`. Cooperative consumers (operations holding `Option<&CancelToken>`)
    /// bail at their next check point with `Err(Error::Cancelled)`.
    ///
    /// Idempotent: calling on an already-cancelled token is a no-op.
    ///
    /// [`is_cancelled`]: Self::is_cancelled
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns `true` once [`cancel`] has been called on this token (or any
    /// clone of it).
    ///
    /// [`cancel`]: Self::cancel
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Helper: checks an optional cancel token and returns `Err(Error::Cancelled)`
/// when set. Use at iteration boundaries inside long operations.
#[inline]
pub(crate) fn bail_if_cancelled(token: Option<&CancelToken>) -> Result<(), crate::PtpError> {
    if let Some(t) = token {
        if t.is_cancelled() {
            return Err(crate::PtpError::Cancelled);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_token_is_not_cancelled() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_flips_the_flag() {
        let token = CancelToken::new();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_is_visible_through_clones() {
        let token = CancelToken::new();
        let clone = token.clone();
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let token = CancelToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn bail_if_cancelled_returns_ok_for_none() {
        assert!(bail_if_cancelled(None).is_ok());
    }

    #[test]
    fn bail_if_cancelled_returns_ok_for_unset_token() {
        let token = CancelToken::new();
        assert!(bail_if_cancelled(Some(&token)).is_ok());
    }

    #[test]
    fn bail_if_cancelled_returns_cancelled_for_set_token() {
        let token = CancelToken::new();
        token.cancel();
        let err = bail_if_cancelled(Some(&token)).unwrap_err();
        assert!(matches!(err, crate::PtpError::Cancelled));
    }
}
