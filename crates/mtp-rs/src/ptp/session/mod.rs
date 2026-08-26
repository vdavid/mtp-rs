//! PTP session management.
//!
//! This module provides session-level operations for MTP/PTP communication.
//! A session maintains the connection state and serializes concurrent operations.

mod operations;
mod properties;
mod streaming;

pub use streaming::{receive_stream_to_stream, ReceiveStream};

use crate::ptp::{
    container_type, pack_u16, pack_u32, unpack_u32, CommandContainer, ContainerType, DataContainer,
    OperationCode, ResponseCode, ResponseContainer, SessionId, TransactionId,
};
use crate::transport::Transport;
use crate::PtpError as Error;
use futures::lock::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// Container header size in bytes.
pub(crate) const HEADER_SIZE: usize = 12;

/// Idle timeout for the bulk-pipe drain performed during desync recovery.
/// Matches [`DEFAULT_CANCEL_TIMEOUT`](crate::mtp::DEFAULT_CANCEL_TIMEOUT) (the
/// value libmtp and Windows use); defined here so the `ptp` layer doesn't
/// depend on the higher-level `mtp` module.
const RECOVERY_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// Shared state that tracks whether the bulk IN pipe may hold an undrained
/// response from a transaction that was abandoned mid-flight.
///
/// MTP/PTP is a strictly serial request→(data)→response protocol over one
/// pair of bulk endpoints. If an operation sends its command but its future
/// is dropped (a superseded listing, a cancelled task, a timeout that races
/// the future) or it returns early before reading the response, the device's
/// reply stays in the pipe. Every later operation then reads the *previous*
/// transaction's reply, so the transaction-ID stream is permanently off by
/// one ("Transaction ID mismatch: expected N, got N-1") and the session is
/// dead until reset.
///
/// `Drop` can't run an async drain (this crate is runtime-agnostic), so
/// recovery is lazy: a dropped/early-exited operation flags the state here,
/// and the next operation drains the pipe under the operation lock before it
/// sends, healing the desync transparently. Shared via `Arc` so a
/// [`ReceiveStream`] dropped mid-download can flag it too.
#[derive(Debug, Default)]
pub(crate) struct RecoveryState {
    /// Whether the pipe needs draining before the next operation.
    needed: AtomicBool,
    /// The transaction ID that was in flight when the pipe was poisoned,
    /// passed to `cancel_transfer` so the drain targets the right transaction.
    poisoned_tx_id: AtomicU32,
}

impl RecoveryState {
    /// Flag the pipe as needing a drain before the next operation, recording
    /// the transaction ID that was in flight.
    fn flag(&self, tx_id: u32) {
        self.poisoned_tx_id.store(tx_id, Ordering::SeqCst);
        self.needed.store(true, Ordering::SeqCst);
    }

    /// If recovery is pending, clear the flag and return the poisoned
    /// transaction ID. Callers hold the operation lock, so this is race-free.
    fn take(&self) -> Option<u32> {
        if self.needed.swap(false, Ordering::SeqCst) {
            Some(self.poisoned_tx_id.load(Ordering::SeqCst))
        } else {
            None
        }
    }

    /// Whether a drain is currently pending.
    fn is_needed(&self) -> bool {
        self.needed.load(Ordering::SeqCst)
    }
}

/// RAII guard armed around an operation's command→response cycle. While armed,
/// dropping it (the operation's future was cancelled, or it returned early via
/// `?` before the response was fully drained and verified) flags the session
/// for recovery. A clean completion calls [`disarm`](Self::disarm) first.
struct TransactionScope<'a> {
    recovery: &'a RecoveryState,
    tx_id: u32,
    armed: bool,
}

impl<'a> TransactionScope<'a> {
    fn arm(recovery: &'a RecoveryState, tx_id: u32) -> Self {
        Self {
            recovery,
            tx_id,
            armed: true,
        }
    }

    /// Mark the transaction as cleanly completed (response fully drained and
    /// transaction ID verified), so dropping the guard won't flag recovery.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TransactionScope<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.recovery.flag(self.tx_id);
        }
    }
}

/// A PTP session with a device.
///
/// PtpSession manages the lifecycle of a PTP/MTP session, including:
/// - Opening and closing sessions
/// - Transaction ID management
/// - Serializing concurrent operations (MTP only allows one operation at a time)
/// - Executing operations and receiving responses
///
/// # Example
///
/// ```rust,no_run
/// use mtp_rs::ptp::PtpDevice;
///
/// # async fn example() -> Result<(), mtp_rs::Error> {
/// let device = PtpDevice::open_first().await?;
/// let session = device.open_session().await?;
///
/// // Get device info
/// let device_info = session.get_device_info().await?;
///
/// // Get storage IDs
/// let storage_ids = session.get_storage_ids().await?;
///
/// // Close the session when done
/// session.close().await?;
/// # Ok(())
/// # }
/// ```
pub struct PtpSession {
    /// The transport layer for USB communication.
    pub(crate) transport: Arc<dyn Transport>,
    /// The session ID assigned to this session.
    session_id: SessionId,
    /// Atomic counter for generating transaction IDs.
    transaction_id: AtomicU32,
    /// Mutex to serialize operations (MTP only allows one operation at a time).
    /// Wrapped in Arc so it can be shared with ReceiveStream.
    pub(crate) operation_lock: Arc<Mutex<()>>,
    /// Whether to send data container headers separately from payloads.
    /// Some devices require the 12-byte PTP header and data payload to arrive
    /// as separate USB bulk transfers.
    split_header_data: AtomicBool,
    /// Tracks whether the bulk pipe needs draining before the next operation
    /// because a prior transaction was abandoned mid-flight. Shared via `Arc`
    /// with [`ReceiveStream`] so a dropped download flags it too.
    recovery: Arc<RecoveryState>,
}

impl PtpSession {
    /// Create a new session (internal, use open() to start session).
    fn new(transport: Arc<dyn Transport>, session_id: SessionId) -> Self {
        Self {
            transport,
            session_id,
            transaction_id: AtomicU32::new(TransactionId::FIRST.0),
            operation_lock: Arc::new(Mutex::new(())),
            split_header_data: AtomicBool::new(false),
            recovery: Arc::new(RecoveryState::default()),
        }
    }

    /// Whether the bulk pipe is currently flagged for a drain before the next
    /// operation (a prior transaction was abandoned mid-flight). The next
    /// operation heals this transparently.
    #[must_use]
    pub fn needs_recovery(&self) -> bool {
        self.recovery.is_needed()
    }

    /// Enable or disable split header/data mode for sending data containers.
    ///
    /// When enabled, the 12-byte PTP container header and payload are sent as
    /// separate USB bulk transfers in [`execute_with_send`](Self::execute_with_send).
    /// Required by some devices that don't handle a combined header+data
    /// bulk transfer.
    pub fn set_split_header_data(&self, split: bool) {
        self.split_header_data.store(split, Ordering::Relaxed);
    }

    /// Whether split header/data mode is currently enabled.
    #[must_use]
    pub fn is_split_header_data(&self) -> bool {
        self.split_header_data.load(Ordering::Relaxed)
    }

    /// Open a new session with the device.
    ///
    /// This sends an OpenSession command to the device and establishes a session
    /// with the given session ID.
    ///
    /// # Arguments
    ///
    /// * `transport` - The transport layer for USB communication
    /// * `session_id` - The session ID to use (typically 1)
    ///
    /// # Errors
    ///
    /// Returns an error if the device rejects the session or communication fails.
    pub async fn open(transport: Arc<dyn Transport>, session_id: u32) -> Result<Self, Error> {
        Self::open_with_reuse(transport, session_id, false).await
    }

    /// Open a session without closing a matching device-side session that already exists.
    ///
    /// This is for devices that keep a session open across host processes and accept the
    /// transaction counter restarting from one. Unlike [`open`](Self::open), a
    /// [`ResponseCode::SessionAlreadyOpen`] response is treated as success and no `CloseSession`
    /// command is sent.
    pub async fn open_reusing_existing(
        transport: Arc<dyn Transport>,
        session_id: u32,
    ) -> Result<Self, Error> {
        Self::open_with_reuse(transport, session_id, true).await
    }

    async fn open_with_reuse(
        transport: Arc<dyn Transport>,
        session_id: u32,
        reuse_existing: bool,
    ) -> Result<Self, Error> {
        let session = Self::new(transport, SessionId(session_id));

        // PTP spec: OpenSession is a session-less operation, so use tx_id=0.
        // Some devices (e.g. Amazon Kindle) enforce this strictly and reject
        // OpenSession with tx_id != 0 as InvalidParameter.
        let response = Self::send_open_session(&session.transport, session_id).await?;

        if response.code == ResponseCode::Ok {
            return Ok(session);
        }

        if response.code == ResponseCode::SessionAlreadyOpen {
            if reuse_existing {
                diag_debug!(
                    "open: device reports SessionAlreadyOpen (id={}); reusing existing session",
                    session_id
                );
                return Ok(session);
            }

            diag_debug!(
                "open: device reports SessionAlreadyOpen (id={}); closing and reopening. \
                 If this persists right after a cancel-wedge reset (#18), the caller reopened \
                 too soon: wait a few seconds quiet, then retry",
                session_id
            );
            // Session already exists with potentially mismatched transaction ID.
            // Close the existing session (ignore errors) and open a fresh one.
            let _ = session.execute(OperationCode::CloseSession, &[]).await;

            // Create a new session instance with reset transaction ID counter
            let fresh_session = Self::new(Arc::clone(&session.transport), SessionId(session_id));

            let retry_response =
                Self::send_open_session(&fresh_session.transport, session_id).await?;

            if retry_response.code != ResponseCode::Ok {
                return Err(Error::Protocol {
                    code: retry_response.code,
                    operation: OperationCode::OpenSession,
                });
            }

            return Ok(fresh_session);
        }

        Err(Error::Protocol {
            code: response.code,
            operation: OperationCode::OpenSession,
        })
    }

    /// Send OpenSession with transaction_id=0 (SESSION_LESS) per the PTP spec.
    async fn send_open_session(
        transport: &Arc<dyn Transport>,
        session_id: u32,
    ) -> Result<ResponseContainer, Error> {
        let cmd = CommandContainer {
            code: OperationCode::OpenSession,
            transaction_id: TransactionId::SESSION_LESS.0,
            params: vec![session_id],
        };
        transport.send_bulk(&cmd.to_bytes()).await?;

        let response_bytes = transport.receive_bulk(512).await?;
        ResponseContainer::from_bytes(&response_bytes)
    }

    /// Get the session ID.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Close the session.
    ///
    /// This sends a CloseSession command to the device. Errors during close
    /// are ignored since the session is being terminated anyway.
    pub async fn close(self) -> Result<(), Error> {
        let _ = self.execute(OperationCode::CloseSession, &[]).await;
        Ok(())
    }

    /// Get the next transaction ID.
    ///
    /// Transaction IDs start at 1 and wrap correctly, skipping 0 and 0xFFFFFFFF.
    pub(crate) fn next_transaction_id(&self) -> u32 {
        loop {
            let current = self.transaction_id.load(Ordering::SeqCst);
            let next = TransactionId(current).next().0;
            if self
                .transaction_id
                .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return current;
            }
        }
    }

    /// Drain the bulk pipe if a prior transaction was abandoned mid-flight,
    /// leaving the session clean before the next command goes out. The caller
    /// must hold the operation lock (so the flag check is race-free). A cheap
    /// no-op when nothing is pending.
    async fn recover_if_needed(&self) -> Result<(), Error> {
        let Some(tx_id) = self.recovery.take() else {
            return Ok(());
        };
        diag_debug!(
            "recover_if_needed: draining pipe for abandoned txn={} before next op",
            tx_id
        );
        // Drain via the validated SIC cancel path (CLASS_CANCEL, then read the
        // bulk IN and interrupt pipes until idle). After this the session is
        // clean and the transaction-ID stream realigns on the next command.
        if let Err(e) = self
            .transport
            .cancel_transfer(tx_id, RECOVERY_DRAIN_TIMEOUT)
            .await
        {
            // Drain failed, so the pipe is still dirty. Re-flag so the next
            // operation retries recovery rather than sending into a poisoned
            // pipe and desyncing.
            self.recovery.flag(tx_id);
            return Err(e);
        }
        Ok(())
    }

    // =========================================================================
    // Core operation execution
    // =========================================================================

    /// Execute a PTP operation without a data phase.
    ///
    /// Exposed for vendor-specific or otherwise non-standard PTP operations
    /// that aren't covered by the high-level API. Obtain a session via
    /// [`PtpDevice::open_session()`](crate::ptp::PtpDevice::open_session).
    pub async fn execute(
        &self,
        operation: OperationCode,
        params: &[u32],
    ) -> Result<ResponseContainer, Error> {
        let _guard = self.operation_lock.lock().await;
        self.recover_if_needed().await?;

        let tx_id = self.next_transaction_id();
        // Armed across the whole command→response cycle: any early exit (a
        // dropped future, an I/O error, a transaction-ID mismatch) leaves the
        // pipe potentially dirty and flags the session for recovery.
        let mut scope = TransactionScope::arm(&self.recovery, tx_id);

        // Build and send command
        let cmd = CommandContainer {
            code: operation,
            transaction_id: tx_id,
            params: params.to_vec(),
        };
        self.transport.send_bulk(&cmd.to_bytes()).await?;

        // Receive response
        let response_bytes = self.transport.receive_bulk(512).await?;
        let response = ResponseContainer::from_bytes(&response_bytes)?;

        // Verify transaction ID matches
        if response.transaction_id != tx_id {
            return Err(Error::invalid_data(format!(
                "Transaction ID mismatch: expected {}, got {}",
                tx_id, response.transaction_id
            )));
        }

        diag_trace!(
            "execute: op={:?} txn={} -> {:?}",
            operation,
            tx_id,
            response.code
        );
        scope.disarm();
        Ok(response)
    }

    /// Execute a PTP operation with a data receive phase.
    ///
    /// Returns the response container along with the received data payload.
    /// Useful for vendor-specific operations that return data.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::ptp::{OperationCode, PtpDevice, ResponseCode};
    ///
    /// # async fn example() -> Result<(), mtp_rs::PtpError> {
    /// let device = PtpDevice::open_first().await?;
    /// let session = device.open_session().await?;
    ///
    /// // Execute a vendor-specific operation (0x9501)
    /// let (response, data) = session
    ///     .execute_with_receive(OperationCode::Unknown(0x9501), &[])
    ///     .await?;
    ///
    /// if response.code == ResponseCode::Ok {
    ///     println!("received {} bytes", data.len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_with_receive(
        &self,
        operation: OperationCode,
        params: &[u32],
    ) -> Result<(ResponseContainer, Vec<u8>), Error> {
        let _guard = self.operation_lock.lock().await;
        self.recover_if_needed().await?;

        let tx_id = self.next_transaction_id();
        let mut scope = TransactionScope::arm(&self.recovery, tx_id);

        // Send command
        let cmd = CommandContainer {
            code: operation,
            transaction_id: tx_id,
            params: params.to_vec(),
        };
        self.transport.send_bulk(&cmd.to_bytes()).await?;

        // Receive data container(s)
        // MTP sends data in one or more containers, then response.
        // A single data container may span multiple USB transfers if larger than 64KB.
        let mut data = Vec::new();

        loop {
            let mut bytes = self.transport.receive_bulk(64 * 1024).await?;
            if bytes.is_empty() {
                return Err(Error::invalid_data("Empty response"));
            }

            let ct = container_type(&bytes)?;
            match ct {
                ContainerType::Data => {
                    // Check if we need to receive more data for this container.
                    // The length field in the header tells us the total container size.
                    if bytes.len() >= 4 {
                        let total_length = unpack_u32(&bytes[0..4])? as usize;
                        // Keep receiving until we have the complete container
                        while bytes.len() < total_length {
                            let more = self.transport.receive_bulk(64 * 1024).await?;
                            if more.is_empty() {
                                return Err(Error::invalid_data(
                                    "Incomplete data container: device stopped sending",
                                ));
                            }
                            bytes.extend_from_slice(&more);
                        }
                    }
                    let container = DataContainer::from_bytes(&bytes)?;
                    data.extend_from_slice(&container.payload);
                    // Continue to receive more containers or response
                }
                ContainerType::Response => {
                    let response = ResponseContainer::from_bytes(&bytes)?;
                    if response.transaction_id != tx_id {
                        return Err(Error::invalid_data(format!(
                            "Transaction ID mismatch: expected {}, got {}",
                            tx_id, response.transaction_id
                        )));
                    }
                    diag_trace!(
                        "execute_with_receive: op={:?} txn={} -> {:?} ({} data bytes)",
                        operation,
                        tx_id,
                        response.code,
                        data.len()
                    );
                    scope.disarm();
                    return Ok((response, data));
                }
                _ => {
                    return Err(Error::invalid_data(format!(
                        "Unexpected container type: {:?}",
                        ct
                    )));
                }
            }
        }
    }

    /// Execute a PTP operation with a data send phase.
    ///
    /// Sends the provided payload to the device as part of the operation.
    /// Useful for vendor-specific operations that require sending data.
    ///
    /// When [`is_split_header_data`](Self::is_split_header_data) is enabled, the
    /// 12-byte PTP container header and the payload are sent as separate USB
    /// bulk transfers.
    pub async fn execute_with_send(
        &self,
        operation: OperationCode,
        params: &[u32],
        data: &[u8],
    ) -> Result<ResponseContainer, Error> {
        let _guard = self.operation_lock.lock().await;
        self.recover_if_needed().await?;

        let tx_id = self.next_transaction_id();
        let mut scope = TransactionScope::arm(&self.recovery, tx_id);

        // Send command
        let cmd = CommandContainer {
            code: operation,
            transaction_id: tx_id,
            params: params.to_vec(),
        };
        self.transport.send_bulk(&cmd.to_bytes()).await?;

        // Send data container
        if self.split_header_data.load(Ordering::Relaxed) {
            // Split mode: send the 12-byte header and the payload as two
            // separate USB bulk transfers. Required by some devices.
            let total_len = (HEADER_SIZE + data.len()) as u32;
            let mut header = Vec::with_capacity(HEADER_SIZE);
            header.extend_from_slice(&pack_u32(total_len));
            header.extend_from_slice(&pack_u16(ContainerType::Data.to_code()));
            header.extend_from_slice(&pack_u16(operation.into()));
            header.extend_from_slice(&pack_u32(tx_id));
            self.transport.send_bulk(&header).await?;
            if !data.is_empty() {
                self.transport.send_bulk(data).await?;
            }
        } else {
            let data_container = DataContainer {
                code: operation,
                transaction_id: tx_id,
                payload: data.to_vec(),
            };
            self.transport.send_bulk(&data_container.to_bytes()).await?;
        }

        // Receive response
        let response_bytes = self.transport.receive_bulk(512).await?;
        let response = ResponseContainer::from_bytes(&response_bytes)?;

        if response.transaction_id != tx_id {
            return Err(Error::invalid_data(format!(
                "Transaction ID mismatch: expected {}, got {}",
                tx_id, response.transaction_id
            )));
        }

        diag_trace!(
            "execute_with_send: op={:?} txn={} ({} data bytes) -> {:?}",
            operation,
            tx_id,
            data.len(),
            response.code
        );
        scope.disarm();
        Ok(response)
    }

    // =========================================================================
    // Helper methods
    // =========================================================================

    /// Helper to check response is OK.
    pub(crate) fn check_response(
        response: &ResponseContainer,
        operation: OperationCode,
    ) -> Result<(), Error> {
        if response.code == ResponseCode::Ok {
            Ok(())
        } else {
            Err(Error::Protocol {
                code: response.code,
                operation,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::{pack_u16, pack_u32, ContainerType, ObjectHandle};
    use crate::transport::mock::MockTransport;

    /// Create a mock transport as Arc<dyn Transport>.
    pub(crate) fn mock_transport() -> (Arc<dyn Transport>, Arc<MockTransport>) {
        let mock = Arc::new(MockTransport::new());
        let transport: Arc<dyn Transport> = Arc::clone(&mock) as Arc<dyn Transport>;
        (transport, mock)
    }

    /// Build an OK response container bytes.
    pub(crate) fn ok_response(tx_id: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&pack_u32(12)); // length
        buf.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
        buf.extend_from_slice(&pack_u16(ResponseCode::Ok.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        buf
    }

    /// Build a response container with params.
    pub(crate) fn response_with_params(tx_id: u32, code: ResponseCode, params: &[u32]) -> Vec<u8> {
        let len = 12 + params.len() * 4;
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(&pack_u32(len as u32));
        buf.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
        buf.extend_from_slice(&pack_u16(code.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        for p in params {
            buf.extend_from_slice(&pack_u32(*p));
        }
        buf
    }

    /// Build a data container.
    pub(crate) fn data_container(tx_id: u32, code: OperationCode, payload: &[u8]) -> Vec<u8> {
        let len = 12 + payload.len();
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(&pack_u32(len as u32));
        buf.extend_from_slice(&pack_u16(ContainerType::Data.to_code()));
        buf.extend_from_slice(&pack_u16(code.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        buf.extend_from_slice(payload);
        buf
    }

    #[tokio::test]
    async fn test_open_session() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession is session-less (tx_id=0)

        let session = PtpSession::open(transport, 1).await.unwrap();
        assert_eq!(session.session_id(), SessionId(1));
    }

    /// Dropping an operation's future after it sent its command but before it
    /// drained the response must flag the session for recovery. Without this,
    /// the device's reply stays in the bulk pipe and desyncs every later op.
    #[tokio::test]
    async fn abandoned_receive_flags_session_for_recovery() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        let session = PtpSession::open(transport, 1).await.unwrap();

        // The device's reply is queued, but the op is abandoned before reading
        // it, modeling a superseded/cancelled listing.
        mock.queue_response(data_container(1, OperationCode::GetObjectInfo, &[0u8; 4]));
        mock.queue_response(ok_response(1));
        mock.block_receive();

        let mut fut = Box::pin(session.execute_with_receive(OperationCode::GetObjectInfo, &[42]));
        assert!(
            matches!(futures::poll!(fut.as_mut()), std::task::Poll::Pending),
            "op should suspend at receive_bulk after sending its command",
        );
        drop(fut);

        assert!(
            session.needs_recovery(),
            "abandoning an op mid-receive must flag the session for recovery",
        );
    }

    /// The real-world bug: after an abandoned op leaves its reply in the pipe,
    /// the next op must not inherit it (a permanent transaction-ID desync).
    /// The session drains the pipe first, so the next op starts clean.
    #[tokio::test]
    async fn abandoned_receive_does_not_poison_next_operation() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        let session = PtpSession::open(transport, 1).await.unwrap();

        // Op A (tx=1): queue its reply, then abandon it mid-receive so the
        // reply is left sitting in the pipe.
        mock.queue_response(data_container(1, OperationCode::GetObjectInfo, &[0u8; 4]));
        mock.queue_response(ok_response(1));
        mock.block_receive();
        let mut op_a = Box::pin(session.execute_with_receive(OperationCode::GetObjectInfo, &[1]));
        assert!(matches!(
            futures::poll!(op_a.as_mut()),
            std::task::Poll::Pending
        ));
        drop(op_a);
        mock.unblock_receive();

        // Op B (tx=2). Before the fix it reads op A's leftover tx=1 reply and
        // dies with a transaction-ID mismatch. After the fix, the session
        // drains the pipe first (one cancel_transfer for tx=1), so op B sends
        // into a clean pipe and finds it empty (NoDevice), never a desync.
        let result = session
            .execute_with_receive(OperationCode::GetObjectInfo, &[2])
            .await;

        assert!(
            matches!(result, Err(Error::NoDevice)),
            "op B must not inherit op A's response; got {result:?}",
        );
        assert_eq!(
            mock.get_cancel_calls(),
            vec![1],
            "recovery must drain the pipe for the poisoned transaction (tx=1)",
        );
    }

    #[tokio::test]
    async fn test_open_session_already_open_recovers() {
        let (transport, mock) = mock_transport();

        // First OpenSession returns SessionAlreadyOpen (session-less, tx_id=0)
        mock.queue_response(response_with_params(
            0,
            ResponseCode::SessionAlreadyOpen,
            &[],
        ));
        // CloseSession (tx_id=1, first counter value)
        mock.queue_response(ok_response(1));
        // Second OpenSession on fresh session (session-less, tx_id=0)
        mock.queue_response(ok_response(0));

        // Should succeed by closing and reopening
        let session = PtpSession::open(transport, 1).await.unwrap();
        assert_eq!(session.session_id(), SessionId(1));
    }

    #[tokio::test]
    async fn test_open_session_already_open_reuses_without_close() {
        let (transport, mock) = mock_transport();
        mock.queue_response(response_with_params(
            0,
            ResponseCode::SessionAlreadyOpen,
            &[],
        ));

        let session = PtpSession::open_reusing_existing(transport, 0xBAAA_AAAD)
            .await
            .unwrap();

        assert_eq!(session.session_id(), SessionId(0xBAAA_AAAD));
        assert_eq!(mock.get_sends().len(), 1, "must not send CloseSession");
    }

    #[tokio::test]
    async fn test_open_session_already_open_transaction_id_reset() {
        let (transport, mock) = mock_transport();

        // First OpenSession returns SessionAlreadyOpen (session-less, tx_id=0)
        mock.queue_response(response_with_params(
            0,
            ResponseCode::SessionAlreadyOpen,
            &[],
        ));
        // CloseSession uses tx_id=1 (first counter value, OpenSession didn't consume one)
        mock.queue_response(ok_response(1));
        // Second OpenSession on fresh session (session-less, tx_id=0)
        mock.queue_response(ok_response(0));
        // First operation on fresh session uses tx_id=1
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();

        // Verify the fresh session's counter starts at 1
        session.delete_object(ObjectHandle(1)).await.unwrap();
    }

    #[tokio::test]
    async fn test_open_session_already_open_close_error_ignored() {
        let (transport, mock) = mock_transport();

        // First OpenSession returns SessionAlreadyOpen (session-less, tx_id=0)
        mock.queue_response(response_with_params(
            0,
            ResponseCode::SessionAlreadyOpen,
            &[],
        ));
        // CloseSession returns an error (tx_id=1, should be ignored)
        mock.queue_response(response_with_params(1, ResponseCode::GeneralError, &[]));
        // Second OpenSession succeeds (session-less, tx_id=0)
        mock.queue_response(ok_response(0));

        // Should succeed even if CloseSession fails
        let session = PtpSession::open(transport, 1).await.unwrap();
        assert_eq!(session.session_id(), SessionId(1));
    }

    #[tokio::test]
    async fn test_open_session_error() {
        let (transport, mock) = mock_transport();
        mock.queue_response(response_with_params(0, ResponseCode::GeneralError, &[]));

        let result = PtpSession::open(transport, 1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_transaction_id_increment() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession (session-less, tx_id=0)
        mock.queue_response(ok_response(1)); // First operation
        mock.queue_response(ok_response(2)); // Second operation

        let session = PtpSession::open(transport, 1).await.unwrap();

        // First post-open operation uses tx_id=1, second uses tx_id=2
        session.delete_object(ObjectHandle(1)).await.unwrap();
        session.delete_object(ObjectHandle(2)).await.unwrap();
    }

    #[tokio::test]
    async fn test_transaction_id_mismatch() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession (session-less, tx_id=0)
        mock.queue_response(ok_response(999)); // Wrong transaction ID

        let session = PtpSession::open(transport, 1).await.unwrap();
        let result = session.delete_object(ObjectHandle(1)).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_with_send_combined_default() {
        // By default, command + combined data container = 2 bulk sends.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // operation response

        let session = PtpSession::open(transport, 1).await.unwrap();
        assert!(!session.is_split_header_data());

        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
        session
            .execute_with_send(OperationCode::SendObject, &[], &payload)
            .await
            .unwrap();

        // 1 send for OpenSession command + 2 here = 3 total.
        let sends = mock.get_sends();
        assert_eq!(sends.len(), 3);

        // Third send is the combined data container: 12-byte header + 4-byte payload.
        let data = &sends[2];
        assert_eq!(data.len(), HEADER_SIZE + payload.len());
        assert_eq!(unpack_u32(&data[0..4]).unwrap() as usize, data.len());
        assert_eq!(&data[HEADER_SIZE..], payload.as_slice());
    }

    #[tokio::test]
    async fn test_execute_with_send_split_header_data() {
        // With split mode enabled, header and payload are sent as 2 separate transfers.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // operation response

        let session = PtpSession::open(transport, 1).await.unwrap();
        session.set_split_header_data(true);
        assert!(session.is_split_header_data());

        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
        session
            .execute_with_send(OperationCode::SendObject, &[], &payload)
            .await
            .unwrap();

        // OpenSession (1) + command (1) + header (1) + payload (1) = 4 sends.
        let sends = mock.get_sends();
        assert_eq!(sends.len(), 4);

        // Third send is the bare 12-byte header reporting the full container length.
        let header = &sends[2];
        assert_eq!(header.len(), HEADER_SIZE);
        assert_eq!(
            unpack_u32(&header[0..4]).unwrap() as usize,
            HEADER_SIZE + payload.len()
        );

        // Fourth send is the raw payload, no extra framing.
        assert_eq!(sends[3], payload);
    }

    #[tokio::test]
    async fn test_execute_with_send_split_empty_payload() {
        // With split mode and an empty payload, only the header is sent (no second transfer).
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        session.set_split_header_data(true);

        session
            .execute_with_send(OperationCode::SendObject, &[], &[])
            .await
            .unwrap();

        // OpenSession + command + header only = 3.
        let sends = mock.get_sends();
        assert_eq!(sends.len(), 3);
        assert_eq!(sends[2].len(), HEADER_SIZE);
    }

    #[tokio::test]
    async fn test_execute_with_send_stream_combined_default() {
        // Combined mode batches header + data into 1 MB USB transfers.
        // A small payload fits entirely in one batch.
        use bytes::Bytes;
        use futures::stream;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // operation response

        let session = PtpSession::open(transport, 1).await.unwrap();
        assert!(!session.is_split_header_data());

        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(&[0xAA, 0xBB])),
            Ok(Bytes::from_static(&[0xCC, 0xDD])),
        ];
        let total_size = 4u64;

        session
            .execute_with_send_stream(
                OperationCode::SendObject,
                &[],
                total_size,
                stream::iter(chunks),
            )
            .await
            .unwrap();

        // OpenSession (1) + command (1) + one batch with header+all data (1) = 3 sends.
        let sends = mock.get_sends();
        assert_eq!(sends.len(), 3);

        let data = &sends[2];
        assert_eq!(data.len(), HEADER_SIZE + total_size as usize);
        assert_eq!(unpack_u32(&data[0..4]).unwrap() as usize, data.len());
        assert_eq!(&data[HEADER_SIZE..], &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[tokio::test]
    async fn test_execute_with_send_stream_combined_large_multichunk() {
        // Verify combined mode works with a large payload (3 MB across 48
        // chunks of 64KB each). The mock transport uses the default
        // send_bulk_streaming which buffers everything into one send_bulk
        // call, so we get a single data send. Real USB transports stream
        // in 256KB transfers.
        use bytes::Bytes;
        use futures::stream;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // operation response

        let session = PtpSession::open(transport, 1).await.unwrap();

        let chunk_size = 64 * 1024;
        let num_chunks = 48; // 3 MB total
        let total_size = (chunk_size * num_chunks) as u64;
        let chunks: Vec<Result<Bytes, std::io::Error>> = (0..num_chunks)
            .map(|i| Ok(Bytes::from(vec![(i % 256) as u8; chunk_size])))
            .collect();

        session
            .execute_with_send_stream(
                OperationCode::SendObject,
                &[],
                total_size,
                stream::iter(chunks),
            )
            .await
            .unwrap();

        // Verify all data arrived (mock buffers everything into one send).
        let sends = mock.get_sends();
        // Skip first 2 sends (OpenSession command + SendObject command).
        let data_sends: Vec<u8> = sends[2..].iter().flat_map(|s| s.clone()).collect();
        assert_eq!(data_sends.len(), HEADER_SIZE + total_size as usize);

        let payload = &data_sends[HEADER_SIZE..];
        for i in 0..num_chunks {
            let chunk_start = i * chunk_size;
            let chunk_end = chunk_start + chunk_size;
            assert!(
                payload[chunk_start..chunk_end]
                    .iter()
                    .all(|&b| b == (i % 256) as u8),
                "chunk {i} data mismatch"
            );
        }
    }

    #[tokio::test]
    async fn test_execute_with_send_stream_split_header_data() {
        // With split mode enabled, the header is sent first as its own bulk
        // transfer and then each non-empty chunk is sent as its own transfer.
        use bytes::Bytes;
        use futures::stream;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // operation response

        let session = PtpSession::open(transport, 1).await.unwrap();
        session.set_split_header_data(true);

        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(&[0xAA, 0xBB])),
            Ok(Bytes::from_static(&[0xCC, 0xDD])),
        ];
        let total_size = 4u64;

        session
            .execute_with_send_stream(
                OperationCode::SendObject,
                &[],
                total_size,
                stream::iter(chunks),
            )
            .await
            .unwrap();

        // OpenSession (1) + command (1) + header (1) + chunk1 (1) + chunk2 (1) = 5.
        let sends = mock.get_sends();
        assert_eq!(sends.len(), 5);

        // Third send is the bare 12-byte header reporting the full container length.
        let header = &sends[2];
        assert_eq!(header.len(), HEADER_SIZE);
        assert_eq!(
            unpack_u32(&header[0..4]).unwrap() as usize,
            HEADER_SIZE + total_size as usize
        );

        // Fourth and fifth sends are the raw chunks, no extra framing.
        assert_eq!(sends[3], &[0xAA, 0xBB]);
        assert_eq!(sends[4], &[0xCC, 0xDD]);
    }

    #[tokio::test]
    async fn test_execute_with_send_stream_split_skips_empty_chunks() {
        // Empty chunks in split mode must not be sent as zero-length bulk
        // transfers (which some devices treat as end-of-transfer markers).
        use bytes::Bytes;
        use futures::stream;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        session.set_split_header_data(true);

        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(&[0xAA])),
            Ok(Bytes::new()), // empty, should be skipped
            Ok(Bytes::from_static(&[0xBB])),
        ];
        let total_size = 2u64;

        session
            .execute_with_send_stream(
                OperationCode::SendObject,
                &[],
                total_size,
                stream::iter(chunks),
            )
            .await
            .unwrap();

        // OpenSession + command + header + chunk1 + chunk3 = 5 (the empty
        // chunk in the middle must not produce a send).
        let sends = mock.get_sends();
        assert_eq!(sends.len(), 5);
        assert_eq!(sends[2].len(), HEADER_SIZE);
        assert_eq!(sends[3], &[0xAA]);
        assert_eq!(sends[4], &[0xBB]);
    }

    #[tokio::test]
    async fn test_close_session() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession (session-less, tx_id=0)
        mock.queue_response(ok_response(1)); // CloseSession (tx_id=1)

        let session = PtpSession::open(transport, 1).await.unwrap();
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_close_session_ignores_errors() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession (session-less, tx_id=0)
        mock.queue_response(response_with_params(1, ResponseCode::GeneralError, &[])); // CloseSession error (tx_id=1)

        let session = PtpSession::open(transport, 1).await.unwrap();
        // Should succeed even if close fails
        session.close().await.unwrap();
    }
}
