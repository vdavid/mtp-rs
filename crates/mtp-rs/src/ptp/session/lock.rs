//! The session's operation lock, with a way for a paced transfer to let other calls in.
//!
//! `futures::lock::Mutex` isn't fair: unlocking wakes a waiter but doesn't hand the lock to it, so
//! a transfer that releases the lock between windows and asks again at once often gets it back.
//! A bare yield between windows was enough on a single-thread executor, where the woken caller
//! runs first. On tokio's multi-thread runtime it wasn't, two ways:
//!
//! - The unlock puts the woken caller in the worker's LIFO slot, the transfer's own wake bumps it
//!   out, and the transfer runs first: on the virtual device a listing waited 80–255 windows.
//! - A call is often several requests, each taking the lock on its own: an Android folder listing
//!   asks `GetObjectPropList` once per property. The first request got in after the window, but
//!   the transfer, woken by that request's release on another worker, beat the next one. On a
//!   Pixel 9 Pro XL a listing waited out two or three 2 s upload windows (2.7–3.4 s, 2026-11-11).
//!
//! So [`OperationLock::let_waiting_calls_in`] waits, when the device is in use or a call is
//! waiting, until it has been quiet for [`HANDOFF_SETTLE`]: no holder, nobody waiting, and no
//! call between its requests.
//!
//! The settle time alone only covers a caller that sends its next request within it. On a starved
//! host the caller's thread can be off the CPU for longer, and the transfer saw a quiet device and
//! went on. So a call that finishes inside one API call marks itself in progress for its whole
//! length ([`OperationLock::begin_call`]), and the handoff waits for it, still capped at
//! [`HANDOFF_CAP`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::lock::{Mutex, MutexGuard, OwnedMutexGuard};

/// How often [`OperationLock::let_waiting_calls_in`] looks whether the device is quiet.
const HANDOFF_POLL: Duration = Duration::from_millis(1);

/// How long the device has to stay quiet before the transfer goes on: longer than the host's own
/// work between the requests of one call, so a multi-request call finishes first.
const HANDOFF_SETTLE: Duration = Duration::from_millis(3);

/// The longest [`OperationLock::let_waiting_calls_in`] waits. Calls that keep the device busy
/// back to back (a tight loop of listings, another transfer) mustn't stop the transfer: after
/// this it queues for the lock like any call.
const HANDOFF_CAP: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(crate) struct OperationLock {
    mutex: Arc<Mutex<()>>,
    /// Calls waiting in [`lock`](Self::lock) or [`lock_owned`](Self::lock_owned).
    waiting: AtomicUsize,
    /// Calls between their first and last request (see [`begin_call`](Self::begin_call)).
    calls: AtomicUsize,
}

/// Marks a call of several requests as in progress, from its first request to its last, so a
/// transfer's handoff waits for the whole call even while the call holds nothing between two
/// requests (see [`OperationLock::begin_call`]). Stops counting when dropped, a dropped call
/// included.
#[must_use = "the call counts as in progress only while the guard lives"]
pub(crate) struct CallGuard(Arc<OperationLock>);

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.0.calls.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Counts one waiting call for as long as it waits, a dropped one included.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl OperationLock {
    pub(crate) async fn lock(&self) -> MutexGuard<'_, ()> {
        let _waiting = Waiting::new(&self.waiting);
        self.mutex.lock().await
    }

    pub(crate) async fn lock_owned(&self) -> OwnedMutexGuard<()> {
        let _waiting = Waiting::new(&self.waiting);
        Arc::clone(&self.mutex).lock_owned().await
    }

    /// Start a call that makes several requests and finishes inside one API call (a collected
    /// listing, an `ObjectInfo` with its size lookup, the start of a windowed upload). Until the
    /// guard drops, [`let_waiting_calls_in`](Self::let_waiting_calls_in) treats the device as
    /// busy, so a transfer can't slip a window in between the call's requests when the caller's
    /// thread is slow to send the next one (a starved host: on a virtual device at a load of
    /// 60–100, a root listing let 14 windows in between its two requests).
    ///
    /// Only for calls that end on their own. A streaming listing makes one request per `next()`
    /// and the consumer's work runs in between, so it takes no guard: a slow consumer would hold
    /// every window of a transfer to [`HANDOFF_CAP`]. A transfer never takes one either: it would
    /// wait on itself.
    pub(crate) fn begin_call(self: &Arc<Self>) -> CallGuard {
        self.calls.fetch_add(1, Ordering::SeqCst);
        CallGuard(Arc::clone(self))
    }

    /// Nobody holds the lock, nobody waits for it, and no call is between its requests. The probe
    /// takes the lock for an instant; a call that asks in that instant is woken when it's
    /// released, like after any holder.
    fn is_quiet(&self) -> bool {
        self.waiting.load(Ordering::SeqCst) == 0
            && self.calls.load(Ordering::SeqCst) == 0
            && self.mutex.try_lock().is_some()
    }

    /// Called between the windows of a transfer, with the lock released. With the device quiet,
    /// yield once and go on, so an uncontended transfer pays nothing. Otherwise wait until the
    /// device has been quiet for [`HANDOFF_SETTLE`], at most [`HANDOFF_CAP`]. Runtime-agnostic:
    /// the waits are `futures_timer` delays.
    pub(crate) async fn let_waiting_calls_in(&self) {
        if self.is_quiet() {
            yield_once().await;
            if self.is_quiet() {
                return;
            }
        }
        let started = Instant::now();
        let mut quiet_since: Option<Instant> = None;
        while started.elapsed() < HANDOFF_CAP {
            if self.is_quiet() {
                if quiet_since.get_or_insert_with(Instant::now).elapsed() >= HANDOFF_SETTLE {
                    return;
                }
            } else {
                quiet_since = None;
            }
            futures_timer::Delay::new(HANDOFF_POLL).await;
        }
    }
}

/// Returns `Pending` once, so tasks the executor already has ready run first.
async fn yield_once() {
    let mut yielded = false;
    futures::future::poll_fn(|cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn with_nobody_waiting_it_returns_at_once() {
        let lock = OperationLock::default();
        let started = Instant::now();
        lock.let_waiting_calls_in().await;
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiting_call_gets_the_lock_before_the_transfer_takes_it_again() {
        let lock = Arc::new(OperationLock::default());
        let guard = lock.lock().await;
        let waiter = {
            let lock = Arc::clone(&lock);
            tokio::spawn(async move {
                let _g = lock.lock().await;
                Instant::now()
            })
        };
        while lock.waiting.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        drop(guard);
        lock.let_waiting_calls_in().await;
        let _again = lock.lock().await;
        let retaken = Instant::now();
        let waiter_got_it = waiter.await.unwrap();
        assert!(
            waiter_got_it <= retaken,
            "the transfer took the lock back first"
        );
    }

    /// A listing on Android is several requests in a row (one `GetObjectPropList` per property),
    /// each taking the lock on its own. All of them get in before the transfer's next window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_call_of_several_requests_finishes_before_the_transfer_goes_on() {
        let lock = Arc::new(OperationLock::default());
        let guard = lock.lock().await;
        let caller = {
            let lock = Arc::clone(&lock);
            tokio::spawn(async move {
                for _ in 0..5 {
                    {
                        let _g = lock.lock().await;
                        // The device answering this request.
                        futures_timer::Delay::new(Duration::from_millis(5)).await;
                    }
                    // The host reading the answer before the next request: on a multi-thread
                    // runtime a transfer woken by the release takes the lock in this gap.
                    tokio::task::yield_now().await;
                }
                Instant::now()
            })
        };
        while lock.waiting.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        drop(guard);
        // Give the caller its first request before the handoff, as on a Pixel: the release wakes
        // it while the transfer finishes up the window.
        futures_timer::Delay::new(Duration::from_millis(1)).await;
        lock.let_waiting_calls_in().await;
        let retaken = {
            let _again = lock.lock().await;
            Instant::now()
        };
        let caller_done = caller.await.unwrap();
        assert!(
            caller_done <= retaken,
            "the transfer went on before the call's last request"
        );
    }

    /// A call of several requests whose host is starved between them: the lock is free and
    /// nobody waits, for longer than [`HANDOFF_SETTLE`]. The call in progress still keeps the
    /// transfer back until its last request, or until [`HANDOFF_CAP`] runs out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_call_paused_between_its_requests_still_finishes_first() {
        let lock = Arc::new(OperationLock::default());
        let (go_on, go_on_rx) = futures::channel::oneshot::channel::<()>();
        let (first_done, first_done_rx) = futures::channel::oneshot::channel::<()>();
        let caller = {
            let lock = Arc::clone(&lock);
            tokio::spawn(async move {
                let call = lock.begin_call();
                drop(lock.lock().await);
                first_done.send(()).unwrap();
                // Starved between requests: nothing held, nothing waiting, for as long as the
                // test says.
                go_on_rx.await.unwrap();
                drop(lock.lock().await);
                drop(call);
                Instant::now()
            })
        };
        first_done_rx.await.unwrap();
        let started = Instant::now();
        let handoff = {
            let lock = Arc::clone(&lock);
            tokio::spawn(async move {
                lock.let_waiting_calls_in().await;
                Instant::now()
            })
        };
        // Let the handoff see a free, unwanted lock for well over the settle time.
        futures_timer::Delay::new(HANDOFF_SETTLE * 10).await;
        go_on.send(()).unwrap();
        let call_done = caller.await.unwrap();
        let handoff_done = handoff.await.unwrap();
        assert!(
            handoff_done >= call_done || handoff_done - started >= HANDOFF_CAP,
            "the transfer went on while a call was between its requests"
        );
    }

    #[tokio::test]
    async fn a_dropped_call_stops_counting() {
        let lock = Arc::new(OperationLock::default());
        let call = lock.begin_call();
        assert!(!lock.is_quiet());
        drop(call);
        assert!(lock.is_quiet());
        // A call dropped mid-request, the way a cancelled listing future is.
        {
            let mut call = Box::pin(async {
                let _call = lock.begin_call();
                let _g = lock.lock().await;
                futures::future::pending::<()>().await;
            });
            assert!(futures::poll!(call.as_mut()).is_pending());
            assert!(!lock.is_quiet());
        }
        assert!(lock.is_quiet());
    }

    #[tokio::test]
    async fn a_dropped_waiter_stops_counting() {
        let lock = OperationLock::default();
        let guard = lock.lock().await;
        {
            let mut waiting = Box::pin(lock.lock());
            assert!(futures::poll!(waiting.as_mut()).is_pending());
            assert_eq!(lock.waiting.load(Ordering::SeqCst), 1);
        }
        assert_eq!(lock.waiting.load(Ordering::SeqCst), 0);
        drop(guard);
    }
}
