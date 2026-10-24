//! Mock transport for testing.

use super::Transport;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Mock transport for testing MTP protocol logic without USB.
///
/// Example:
/// ```rust,ignore
/// let mut mock = MockTransport::new();
/// mock.expect_send(vec![0x10, 0x00, ...]);  // Expected command
/// mock.queue_response(vec![0x0C, 0x00, ...]); // OK response
///
/// // ... run test ...
///
/// mock.verify().expect("Verification failed");
/// ```
pub struct MockTransport {
    expected_sends: Mutex<VecDeque<Vec<u8>>>,
    queued_responses: Mutex<VecDeque<Vec<u8>>>,
    queued_interrupts: Mutex<VecDeque<Vec<u8>>>,
    actual_sends: Mutex<Vec<Vec<u8>>>,
    cancel_calls: Mutex<Vec<u32>>,
    cancel_results: Mutex<VecDeque<Result<(), crate::PtpError>>>,
    /// Replies the device sends only after a `drain_bulk_in`, in answer to commands the host
    /// sends once the pipe is clean. A drain discards everything in `queued_responses`.
    responses_after_drain: Mutex<VecDeque<Vec<u8>>>,
    drain_calls: Mutex<u32>,
    /// When set, the next `receive_bulk` call never resolves (until the caller's
    /// future is dropped). Models a transaction abandoned mid-receive: the
    /// command went out, but the host never reads the device's response.
    block_receive: AtomicBool,
}

impl MockTransport {
    /// Create a new mock transport with no expectations or queued responses.
    #[must_use]
    pub fn new() -> Self {
        Self {
            expected_sends: Mutex::new(VecDeque::new()),
            queued_responses: Mutex::new(VecDeque::new()),
            queued_interrupts: Mutex::new(VecDeque::new()),
            actual_sends: Mutex::new(Vec::new()),
            cancel_calls: Mutex::new(Vec::new()),
            cancel_results: Mutex::new(VecDeque::new()),
            responses_after_drain: Mutex::new(VecDeque::new()),
            drain_calls: Mutex::new(0),
            block_receive: AtomicBool::new(false),
        }
    }

    /// Queue a reply that only becomes readable after the next `drain_bulk_in`.
    pub fn queue_response_after_drain(&self, data: Vec<u8>) {
        self.responses_after_drain.lock().push_back(data);
    }

    /// How many times `drain_bulk_in` ran.
    pub fn drain_calls(&self) -> u32 {
        *self.drain_calls.lock()
    }

    /// Make the next (and every subsequent) `receive_bulk` call hang forever,
    /// until [`unblock_receive`](Self::unblock_receive) is called. Use this to
    /// suspend an operation between sending its command and draining the
    /// response, then drop its future to model a mid-transaction abandonment.
    pub fn block_receive(&self) {
        self.block_receive.store(true, Ordering::SeqCst);
    }

    /// Stop blocking `receive_bulk` (see [`block_receive`](Self::block_receive)).
    pub fn unblock_receive(&self) {
        self.block_receive.store(false, Ordering::SeqCst);
    }

    /// Expect a specific byte sequence to be sent.
    /// If sends don't match expectations, verify() will fail.
    pub fn expect_send(&self, data: Vec<u8>) {
        self.expected_sends.lock().push_back(data);
    }

    /// Queue a response to be returned by receive_bulk().
    pub fn queue_response(&self, data: Vec<u8>) {
        self.queued_responses.lock().push_back(data);
    }

    /// Queue an interrupt response to be returned by receive_interrupt().
    pub fn queue_interrupt(&self, data: Vec<u8>) {
        self.queued_interrupts.lock().push_back(data);
    }

    /// Verify all expected sends occurred and all responses were consumed.
    pub fn verify(&self) -> Result<(), String> {
        let expected = self.expected_sends.lock();
        let responses = self.queued_responses.lock();
        let interrupts = self.queued_interrupts.lock();
        let cancel_results = self.cancel_results.lock();

        let mut errors = Vec::new();

        if !expected.is_empty() {
            errors.push(format!(
                "{} expected send(s) were not received",
                expected.len()
            ));
        }

        if !responses.is_empty() {
            errors.push(format!(
                "{} queued response(s) were not consumed",
                responses.len()
            ));
        }

        if !interrupts.is_empty() {
            errors.push(format!(
                "{} queued interrupt(s) were not consumed",
                interrupts.len()
            ));
        }

        if !cancel_results.is_empty() {
            errors.push(format!(
                "{} queued cancel result(s) were not consumed",
                cancel_results.len()
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Get all data that was actually sent (for inspection in tests).
    pub fn get_sends(&self) -> Vec<Vec<u8>> {
        self.actual_sends.lock().clone()
    }

    /// Queue a result for the next `cancel_transfer` call.
    pub fn queue_cancel_result(&self, result: Result<(), crate::PtpError>) {
        self.cancel_results.lock().push_back(result);
    }

    /// Get all transaction IDs passed to `cancel_transfer`.
    pub fn get_cancel_calls(&self) -> Vec<u32> {
        self.cancel_calls.lock().clone()
    }

    /// Clear all expectations and queued responses.
    pub fn reset(&self) {
        self.expected_sends.lock().clear();
        self.queued_responses.lock().clear();
        self.queued_interrupts.lock().clear();
        self.actual_sends.lock().clear();
        self.cancel_calls.lock().clear();
        self.cancel_results.lock().clear();
        self.responses_after_drain.lock().clear();
    }
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn send_bulk(&self, data: &[u8]) -> Result<(), crate::PtpError> {
        // Store sent data for verification
        self.actual_sends.lock().push(data.to_vec());

        // If expectations exist, verify they match
        let expected = self.expected_sends.lock().pop_front();
        if let Some(expected_data) = expected {
            if data != expected_data.as_slice() {
                return Err(crate::PtpError::invalid_data(format!(
                    "send mismatch: expected {:?}, got {:?}",
                    expected_data, data
                )));
            }
        }

        Ok(())
    }

    async fn receive_bulk(&self, _max_size: usize) -> Result<Vec<u8>, crate::PtpError> {
        // Models a transaction abandoned mid-receive: hang until the caller's
        // future is dropped. `pending()` never resolves, so dropping the
        // outer future is the only way out (exactly what cancellation does).
        if self.block_receive.load(Ordering::SeqCst) {
            futures::future::pending::<()>().await;
        }
        // Return next queued response or error if none
        self.queued_responses
            .lock()
            .pop_front()
            .ok_or(crate::PtpError::NoDevice)
    }

    async fn receive_interrupt(&self) -> Result<Vec<u8>, crate::PtpError> {
        // Return next queued interrupt or error if none
        self.queued_interrupts
            .lock()
            .pop_front()
            .ok_or(crate::PtpError::NoDevice)
    }

    async fn cancel_transfer(
        &self,
        transaction_id: u32,
        _idle_timeout: Duration,
    ) -> Result<(), crate::PtpError> {
        self.cancel_calls.lock().push(transaction_id);
        // Real transports drain the bulk IN and interrupt pipes here, leaving
        // the session clean for the next operation. Model that by discarding
        // any responses still sitting in the pipe.
        self.queued_responses.lock().clear();
        self.queued_interrupts.lock().clear();
        self.cancel_results.lock().pop_front().unwrap_or(Ok(()))
    }

    async fn drain_bulk_in(
        &self,
        _idle_timeout: Duration,
        _max_duration: Duration,
    ) -> Result<usize, crate::PtpError> {
        *self.drain_calls.lock() += 1;
        let mut queued = self.queued_responses.lock();
        let drained = queued.iter().map(Vec::len).sum();
        queued.clear();
        queued.extend(self.responses_after_drain.lock().drain(..));
        Ok(drained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_receive() {
        let mock = MockTransport::new();
        mock.queue_response(vec![1, 2, 3]);

        mock.send_bulk(&[4, 5, 6]).await.unwrap();
        let response = mock.receive_bulk(100).await.unwrap();

        assert_eq!(response, vec![1, 2, 3]);
        assert_eq!(mock.get_sends(), vec![vec![4, 5, 6]]);
    }

    #[tokio::test]
    async fn test_expected_send_matches() {
        let mock = MockTransport::new();
        mock.expect_send(vec![1, 2, 3]);

        mock.send_bulk(&[1, 2, 3]).await.unwrap();
        mock.verify().unwrap();
    }

    #[tokio::test]
    async fn test_expected_send_mismatch() {
        let mock = MockTransport::new();
        mock.expect_send(vec![1, 2, 3]);

        let result = mock.send_bulk(&[1, 2, 4]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_verify_fails_with_unconsumed_expectations() {
        let mock = MockTransport::new();
        mock.expect_send(vec![1, 2, 3]);

        // Don't send anything
        let result = mock.verify();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_verify_fails_with_unconsumed_responses() {
        let mock = MockTransport::new();
        mock.queue_response(vec![1, 2, 3]);

        // Don't receive anything
        let result = mock.verify();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_receive_bulk_empty_queue_returns_error() {
        let mock = MockTransport::new();

        let result = mock.receive_bulk(100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_receive_interrupt() {
        let mock = MockTransport::new();
        mock.queue_interrupt(vec![10, 20, 30]);

        let result = mock.receive_interrupt().await.unwrap();
        assert_eq!(result, vec![10, 20, 30]);
    }

    #[tokio::test]
    async fn test_receive_interrupt_empty_queue_returns_error() {
        let mock = MockTransport::new();

        let result = mock.receive_interrupt().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_reset_clears_all_state() {
        let mock = MockTransport::new();
        mock.expect_send(vec![1, 2, 3]);
        mock.queue_response(vec![4, 5, 6]);
        mock.queue_interrupt(vec![7, 8, 9]);
        mock.send_bulk(&[10, 11, 12]).await.ok();

        mock.reset();

        assert!(mock.get_sends().is_empty());
        mock.verify().unwrap(); // Should pass since everything was cleared
    }

    #[tokio::test]
    async fn test_multiple_sends_and_responses() {
        let mock = MockTransport::new();
        mock.expect_send(vec![1, 2]);
        mock.expect_send(vec![3, 4]);
        mock.queue_response(vec![5, 6]);
        mock.queue_response(vec![7, 8]);

        mock.send_bulk(&[1, 2]).await.unwrap();
        mock.send_bulk(&[3, 4]).await.unwrap();

        let r1 = mock.receive_bulk(100).await.unwrap();
        let r2 = mock.receive_bulk(100).await.unwrap();

        assert_eq!(r1, vec![5, 6]);
        assert_eq!(r2, vec![7, 8]);
        mock.verify().unwrap();
    }

    #[tokio::test]
    async fn test_default_impl() {
        let mock = MockTransport::default();
        mock.queue_response(vec![1, 2, 3]);
        let response = mock.receive_bulk(100).await.unwrap();
        assert_eq!(response, vec![1, 2, 3]);
    }
}
