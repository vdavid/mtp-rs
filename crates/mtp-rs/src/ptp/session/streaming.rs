//! Streaming transfer operations.
//!
//! This module contains the `ReceiveStream` struct and methods for streaming
//! data transfers, allowing memory-efficient downloads and uploads without
//! buffering entire files in memory.

use crate::ptp::{
    container_type, pack_u16, pack_u32, unpack_u32, CommandContainer, ContainerType, ObjectHandle,
    OperationCode, ResponseCode, ResponseContainer,
};
use crate::transport::Transport;
use crate::PtpError as Error;
use bytes::{Buf, Bytes, BytesMut};
use futures::lock::OwnedMutexGuard;
use futures::Stream;
use std::sync::Arc;
use std::time::Duration;

use super::{
    is_stale_reply, may_skip_stale_reply, transaction_mismatch, PtpSession, RecoveryState,
    TransactionScope, HEADER_SIZE,
};

/// What [`ReceiveStream::consume_container_header`] found at the front of the buffer.
enum Header {
    /// A data container; its payload follows.
    Data,
    /// The response that closes this transfer.
    Done,
    /// A late reply to an earlier transaction, dropped from the buffer.
    SkippedStaleReply,
}

/// How much a receiving stream asks the transport for in one bulk read. It holds
/// about this much at a time, whatever the object's size.
const RECEIVE_CHUNK: usize = 64 * 1024;

/// The `ContainerLength` a responder sends when the data phase is bigger than a
/// 32-bit length can express (MTP 1.1 appendix H.1). The header then says nothing
/// about where the phase ends: the short packet does, and the real byte count comes
/// from the object's `ObjectCompressedSize`.
const LARGE_OBJECT_LENGTH_SENTINEL: u32 = 0xFFFF_FFFF;

impl PtpSession {
    // =========================================================================
    // Streaming operations
    // =========================================================================

    /// Execute operation with streaming data receive.
    ///
    /// Returns a Stream that yields data chunks as they arrive from USB.
    /// The stream yields `Bytes` chunks (typically up to 64KB each).
    ///
    /// # Important
    ///
    /// The caller must either consume the entire stream or call
    /// [`cancel()`](ReceiveStream::cancel) before dropping it. The MTP
    /// session is locked while the stream is active.
    ///
    /// # Arguments
    ///
    /// * `operation` - The operation code to execute
    /// * `params` - Operation parameters
    ///
    /// # Returns
    ///
    /// A `ReceiveStream` that yields `Result<Bytes, Error>` chunks.
    pub async fn execute_with_receive_stream(
        self: &Arc<Self>,
        operation: OperationCode,
        params: &[u32],
    ) -> Result<ReceiveStream, Error> {
        self.start_receive_stream(operation, params, None).await
    }

    /// Execute operation with streaming data receive, telling the stream how many
    /// payload bytes to expect.
    ///
    /// Same as [`execute_with_receive_stream`](Self::execute_with_receive_stream)
    /// except for objects over 4 GiB, where the container header carries the
    /// `0xFFFFFFFF` length sentinel instead of a real length (MTP 1.1 appendix H.1)
    /// and the spec points at `ObjectCompressedSize` for the true figure. Passing it
    /// here keeps the end of such a transfer a byte count rather than short-packet
    /// detection alone. For every other container the header's own length wins, so an
    /// inexact value is harmless.
    ///
    /// # Arguments
    ///
    /// * `operation` - The operation code to execute
    /// * `params` - Operation parameters
    /// * `expected_payload_len` - Payload bytes the object is known to hold
    pub async fn execute_with_receive_stream_sized(
        self: &Arc<Self>,
        operation: OperationCode,
        params: &[u32],
        expected_payload_len: u64,
    ) -> Result<ReceiveStream, Error> {
        self.start_receive_stream(operation, params, Some(expected_payload_len))
            .await
    }

    async fn start_receive_stream(
        self: &Arc<Self>,
        operation: OperationCode,
        params: &[u32],
        expected_payload_len: Option<u64>,
    ) -> Result<ReceiveStream, Error> {
        // Clone the Arc for the lock
        let lock = Arc::clone(&self.operation_lock);
        let guard = lock.lock_owned().await;
        self.recover_if_needed().await?;

        let tx_id = self.next_transaction_id();

        // Armed until the stream is constructed, so a failed send flags
        // recovery. Once the stream exists, its own `Drop` owns that duty.
        let mut scope = TransactionScope::arm(&self.recovery, tx_id);

        // Send command
        let cmd = CommandContainer {
            code: operation,
            transaction_id: tx_id,
            params: params.to_vec(),
        };
        self.transport.send_bulk(&cmd.to_bytes()).await?;
        scope.disarm();

        Ok(ReceiveStream {
            transport: Arc::clone(&self.transport),
            recovery: Arc::clone(&self.recovery),
            _guard: guard,
            transaction_id: tx_id,
            operation,
            buffer: BytesMut::new(),
            expected_payload_len,
            payload_remaining: None,
            payload_yielded: 0,
            large_object: false,
            in_payload: false,
            short_read: false,
            expect_zero_length_packet: false,
            done: false,
            stale_replies_skipped: 0,
            saw_data_container: false,
        })
    }

    /// Execute operation with streaming data send.
    ///
    /// Accepts a Stream of data chunks to send. The total_size must be
    /// known upfront (MTP protocol requirement).
    ///
    /// # Arguments
    ///
    /// * `operation` - The operation code
    /// * `params` - Operation parameters
    /// * `total_size` - Total bytes that will be sent (REQUIRED by MTP protocol)
    /// * `data` - Stream of data chunks to send
    ///
    /// # Important
    ///
    /// The `total_size` must match the actual total bytes in the stream.
    /// MTP requires knowing the size before transfer begins.
    ///
    /// `data` must be `Unpin` (wrap it with `Box::pin` if yours isn't) and `Send`, because the
    /// transfer runs on the session's transport. It doesn't need to be `'static`.
    ///
    /// When [`is_split_header_data`](Self::is_split_header_data) is enabled, the
    /// 12-byte PTP container header and the streamed payload are sent as
    /// separate USB bulk transfers, mirroring the behavior of
    /// [`execute_with_send`](Self::execute_with_send).
    pub async fn execute_with_send_stream<S>(
        &self,
        operation: OperationCode,
        params: &[u32],
        total_size: u64,
        mut data: S,
    ) -> Result<ResponseContainer, Error>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send,
    {
        use futures::StreamExt;
        use std::sync::atomic::Ordering;

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

        let container_length = HEADER_SIZE as u64 + total_size;

        // Build the 12-byte data container header.
        let mut header = Vec::with_capacity(HEADER_SIZE);
        if container_length <= u32::MAX as u64 {
            header.extend_from_slice(&pack_u32(container_length as u32));
        } else {
            header.extend_from_slice(&pack_u32(0xFFFFFFFF));
        }
        header.extend_from_slice(&pack_u16(ContainerType::Data.to_code()));
        header.extend_from_slice(&pack_u16(operation.into()));
        header.extend_from_slice(&pack_u32(tx_id));

        if self.split_header_data.load(Ordering::Relaxed) {
            // Split mode: send the header as its own bulk transfer, then send
            // each streamed chunk as its own bulk transfer. Required by some
            // devices that don't handle a combined header+data bulk transfer.
            self.transport.send_bulk(&header).await?;
            while let Some(chunk_result) = data.next().await {
                let chunk = chunk_result.map_err(Error::Io)?;
                if !chunk.is_empty() {
                    self.transport.send_bulk(&chunk).await?;
                }
            }
        } else {
            // Combined mode: stream header + data as one continuous USB
            // transfer. The transport handles buffering and ZLP termination,
            // so we never buffer the entire file in RAM.
            let header_stream = futures::stream::once(async { Ok(Bytes::from(header)) });
            let combined = header_stream.chain(data);
            self.transport
                .send_bulk_streaming(Box::pin(combined))
                .await?;
        }

        let response = self.receive_response(tx_id, operation).await?;

        scope.disarm();
        Ok(response)
    }

    /// Download an object as a stream of chunks.
    ///
    /// This is a convenience method that calls `execute_with_receive_stream`
    /// with GetObject operation.
    ///
    /// # Important
    ///
    /// The caller must either consume the entire stream or call
    /// [`cancel()`](ReceiveStream::cancel) before dropping it. The MTP
    /// session is locked while the stream is active.
    pub async fn get_object_stream(
        self: &Arc<Self>,
        handle: ObjectHandle,
    ) -> Result<ReceiveStream, Error> {
        self.execute_with_receive_stream(OperationCode::GetObject, &[handle.0])
            .await
    }

    /// Upload an object from a stream.
    ///
    /// This is a convenience method that streams object data directly to USB.
    ///
    /// # Arguments
    ///
    /// * `total_size` - Total bytes that will be sent
    /// * `data` - Stream of data chunks to send
    pub async fn send_object_stream<S>(&self, total_size: u64, data: S) -> Result<(), Error>
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send,
    {
        let response = self
            .execute_with_send_stream(OperationCode::SendObject, &[], total_size, data)
            .await?;
        Self::check_response(&response, OperationCode::SendObject)?;
        Ok(())
    }
}

/// A stream of data chunks received from USB during a download operation.
///
/// This stream yields `Bytes` chunks as they arrive from the device,
/// allowing memory-efficient streaming without buffering the entire file.
///
/// # Important
///
/// The MTP session is locked while this stream exists. Prefer to consume the
/// entire stream or call [`cancel()`](Self::cancel) before dropping it:
/// `cancel()` drains the pipe right away. Dropping mid-stream without that is
/// still safe (it flags the session, and the next operation drains the pipe
/// before it runs), but the drain then happens lazily rather than promptly.
#[must_use = "consume a ReceiveStream fully or call cancel() to drain the pipe promptly; \
               dropping it mid-transfer defers the drain to the next operation"]
pub struct ReceiveStream {
    /// The transport layer for USB communication.
    transport: Arc<dyn Transport>,
    /// Shared session recovery state. On a mid-transfer drop, flags the pipe
    /// for draining before the next operation.
    recovery: Arc<RecoveryState>,
    /// Guard that holds the operation lock for the duration of streaming.
    _guard: OwnedMutexGuard<()>,
    /// Transaction ID for this operation.
    transaction_id: u32,
    /// Operation code for this operation.
    operation: OperationCode,
    /// Bytes read from the transport but not yet handed to the caller. Chunks are
    /// split off the front and the space is reclaimed straight away, so this holds
    /// roughly one bulk read, never the whole object.
    buffer: BytesMut,
    /// Payload length the caller told us to expect, used to bound a container that
    /// carries the >4 GiB length sentinel.
    expected_payload_len: Option<u64>,
    /// Payload bytes still expected in the container being streamed. `None` means an
    /// over-4-GiB container is in flight with no caller-supplied size, so the data
    /// phase ends at the next short packet instead of at a byte count.
    payload_remaining: Option<u64>,
    /// Payload bytes already yielded from the container being streamed.
    payload_yielded: u64,
    /// Whether the container being streamed carried the >4 GiB length sentinel.
    large_object: bool,
    /// Whether a data container header has been consumed and its payload is streaming.
    in_payload: bool,
    /// Whether the last bulk read came up short, which ends a data phase.
    short_read: bool,
    /// Whether an empty read is expected next. A data phase whose length divides the
    /// USB packet size is terminated by a zero-length packet, and when the payload also
    /// filled our whole read the device has nothing left to piggyback it on, so it
    /// arrives as a read of its own.
    expect_zero_length_packet: bool,
    /// Late replies to earlier transactions skipped so far (see `is_stale_reply`).
    stale_replies_skipped: u32,
    /// Whether a data container arrived yet; only a reply before any data can be a late one.
    saw_data_container: bool,
    /// Whether the stream is complete.
    done: bool,
}

impl ReceiveStream {
    /// Get the transaction ID for this operation.
    #[must_use]
    pub fn transaction_id(&self) -> u32 {
        self.transaction_id
    }

    /// Poll for the next chunk of data.
    ///
    /// This is the async version of the Stream trait's poll_next.
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, Error>> {
        if self.done {
            return None;
        }

        loop {
            // Between containers: consume the next header once enough of it is here.
            if !self.in_payload && self.buffer.len() >= HEADER_SIZE {
                match self.consume_container_header() {
                    Ok(Header::Data | Header::SkippedStaleReply) => {}
                    Ok(Header::Done) => {
                        self.done = true;
                        return None;
                    }
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                }
            }

            // A >4 GiB container's header can't say where the data phase ends, so the
            // short packet does: whatever is buffered is the last of the payload. The
            // "already yielded something" guard keeps a split-header transfer (header
            // alone in one short read) from reading as an instant end of phase.
            if self.in_payload && self.large_object && self.short_read && self.payload_yielded > 0 {
                let buffered = self.buffer.len() as u64;
                self.payload_remaining = Some(
                    self.payload_remaining
                        .map_or(buffered, |left| left.min(buffered)),
                );
            }

            if self.in_payload {
                let available = self.buffer.len() as u64;
                let take =
                    self.payload_remaining
                        .map_or(available, |left| left.min(available)) as usize;
                if take > 0 {
                    // `split_to` hands the bytes over and advances the front in one go:
                    // no copy, and the buffer never accumulates what we already yielded.
                    let chunk = self.buffer.split_to(take).freeze();
                    self.payload_yielded += take as u64;
                    if let Some(left) = self.payload_remaining.as_mut() {
                        *left -= take as u64;
                    }
                    if self.payload_remaining == Some(0) {
                        self.end_container();
                    }
                    return Some(Ok(chunk));
                }
                if self.payload_remaining == Some(0) {
                    self.end_container();
                    continue;
                }
            }

            // Need more data from USB.
            self.short_read = false;
            match self.transport.receive_bulk(RECEIVE_CHUNK).await {
                Ok(bytes) => {
                    if bytes.is_empty() {
                        // A zero-length packet terminating a data phase: expected right
                        // after one ended, and the end-of-phase signal itself for a
                        // >4 GiB container. Anywhere else it means the device went quiet.
                        if self.in_payload && self.large_object {
                            self.short_read = true;
                        } else if self.expect_zero_length_packet {
                            self.expect_zero_length_packet = false;
                        } else {
                            return Some(Err(Error::invalid_data("Empty response from device")));
                        }
                        continue;
                    }
                    self.expect_zero_length_packet = false;
                    self.short_read = bytes.len() < RECEIVE_CHUNK;
                    self.buffer.extend_from_slice(&bytes);
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }

    /// Consume the container header sitting at the front of the buffer.
    fn consume_container_header(&mut self) -> Result<Header, Error> {
        match container_type(&self.buffer)? {
            ContainerType::Data => {
                let length = unpack_u32(&self.buffer[0..4])?;
                self.buffer.advance(HEADER_SIZE);
                self.large_object = length == LARGE_OBJECT_LENGTH_SENTINEL;
                self.payload_remaining = if self.large_object {
                    // Over 4 GiB: run to the caller's size when it gave us one, else to
                    // the short packet.
                    self.expected_payload_len
                } else if (length as usize) < HEADER_SIZE {
                    return Err(Error::invalid_data(format!(
                        "Data container length {length} is shorter than its {HEADER_SIZE}-byte header"
                    )));
                } else {
                    Some(u64::from(length) - HEADER_SIZE as u64)
                };
                self.payload_yielded = 0;
                self.in_payload = true;
                self.saw_data_container = true;
                Ok(Header::Data)
            }
            ContainerType::Response => {
                let response = ResponseContainer::from_bytes(&self.buffer)?;

                if response.transaction_id != self.transaction_id {
                    if !self.saw_data_container
                        && is_stale_reply(response.transaction_id, self.transaction_id)
                        && may_skip_stale_reply(self.operation, self.stale_replies_skipped)
                    {
                        self.stale_replies_skipped += 1;
                        let length = (unpack_u32(&self.buffer[0..4])? as usize)
                            .clamp(HEADER_SIZE, self.buffer.len());
                        self.buffer.advance(length);
                        return Ok(Header::SkippedStaleReply);
                    }
                    return Err(transaction_mismatch(
                        self.transaction_id,
                        response.transaction_id,
                    ));
                }

                if response.code != ResponseCode::Ok {
                    return Err(Error::Protocol {
                        code: response.code,
                        operation: self.operation,
                    });
                }

                Ok(Header::Done)
            }
            other => Err(Error::invalid_data(format!(
                "Unexpected container type: {other:?}"
            ))),
        }
    }

    /// Finish the container being streamed and go back to expecting a header.
    fn end_container(&mut self) {
        self.expect_zero_length_packet = true;
        self.in_payload = false;
        self.payload_remaining = None;
        self.payload_yielded = 0;
        self.large_object = false;
    }

    /// Cancel the in-progress download.
    ///
    /// Uses the USB Still Image Class cancel mechanism: sends a CLASS_CANCEL
    /// control request to the device, then drains any remaining data from
    /// the USB pipes. The session stays healthy for subsequent operations.
    ///
    /// The `idle_timeout` controls how long to wait during pipe drain before
    /// assuming the pipe is clear. 300ms is the recommended default; see
    /// [`DEFAULT_CANCEL_TIMEOUT`](crate::mtp::DEFAULT_CANCEL_TIMEOUT).
    ///
    /// If the stream is already complete, this is a no-op.
    pub async fn cancel(&mut self, idle_timeout: Duration) -> Result<(), Error> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        let result = self
            .transport
            .cancel_transfer(self.transaction_id, idle_timeout)
            .await;
        // A failed cancel may have left the rest of the object and its response in the
        // pipe, so the next operation drains first (#43). Not after `DeviceReset`: that
        // session is gone, and the caller's reopen needs a quiet device.
        if matches!(&result, Err(e) if !matches!(e, Error::DeviceReset)) {
            self.recovery.flag(self.transaction_id);
        }
        result
    }

    /// Collect all remaining data into a `Vec<u8>`.
    ///
    /// This consumes the stream and buffers all data in memory.
    pub async fn collect(mut self) -> Result<Vec<u8>, Error> {
        let mut data = Vec::new();
        while let Some(result) = self.next_chunk().await {
            let chunk = result?;
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }
}

impl Drop for ReceiveStream {
    fn drop(&mut self) {
        if !self.done {
            // Abandoned mid-transfer without consuming the stream or calling
            // cancel(): the device's data/response is still in the bulk pipe.
            // Flag the session so the next operation drains it before sending,
            // instead of inheriting it and desyncing the transaction-ID stream.
            self.recovery.flag(self.transaction_id);
        }
    }
}

/// Convert a ReceiveStream into a futures::Stream using async iteration.
///
/// This creates a proper Stream that can be used with StreamExt methods.
pub fn receive_stream_to_stream(
    recv: ReceiveStream,
) -> impl Stream<Item = Result<Bytes, Error>> + Send {
    futures::stream::unfold(recv, |mut recv| async move {
        recv.next_chunk().await.map(|result| (result, recv))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::session::tests::{
        data_container, mock_transport, ok_response, response_with_params,
    };
    use crate::ptp::{pack_u16, pack_u32, ResponseCode};
    use std::sync::Mutex;

    /// One bulk read's worth of data, matching what `next_chunk` asks for.
    const BULK_READ: usize = 64 * 1024;

    /// The byte a synthetic object carries at `offset`.
    fn synthetic_byte(offset: u64) -> u8 {
        (offset % 251) as u8
    }

    /// Serves one synthetic `GetObject` data phase without ever materializing the
    /// object, so a test can watch what `ReceiveStream` holds while a transfer far
    /// bigger than its buffer flows through it.
    ///
    /// The header goes out as its own bulk transfer (the split-header shape some
    /// devices use), then the payload in `max_size` reads, then the response
    /// container. A payload that lands on a read boundary is followed by a
    /// zero-length packet, exactly as a real device terminates the data phase.
    struct BigObjectTransport {
        /// What to write in the container's `ContainerLength` field.
        declared_length: u32,
        payload_len: u64,
        tx_id: u32,
        /// Return the response container tacked onto the last payload read instead of
        /// as its own read, so only a byte count can find the end of the payload.
        coalesce_response: bool,
        phase: Mutex<Phase>,
    }

    enum Phase {
        OpenSession,
        Header,
        Payload { served: u64 },
        Response,
        Exhausted,
    }

    impl BigObjectTransport {
        fn serving(declared_length: u32, payload_len: u64, tx_id: u32) -> Arc<dyn Transport> {
            Arc::new(Self {
                declared_length,
                payload_len,
                tx_id,
                coalesce_response: false,
                phase: Mutex::new(Phase::OpenSession),
            })
        }

        fn coalescing_response(
            declared_length: u32,
            payload_len: u64,
            tx_id: u32,
        ) -> Arc<dyn Transport> {
            Arc::new(Self {
                declared_length,
                payload_len,
                tx_id,
                coalesce_response: true,
                phase: Mutex::new(Phase::OpenSession),
            })
        }
    }

    #[async_trait::async_trait]
    impl Transport for BigObjectTransport {
        async fn send_bulk(&self, _data: &[u8]) -> Result<(), Error> {
            Ok(())
        }

        async fn receive_bulk(&self, max_size: usize) -> Result<Vec<u8>, Error> {
            let mut phase = self.phase.lock().unwrap();
            match *phase {
                Phase::OpenSession => {
                    *phase = Phase::Header;
                    Ok(ok_response(0))
                }
                Phase::Header => {
                    *phase = Phase::Payload { served: 0 };
                    let mut header = Vec::with_capacity(HEADER_SIZE);
                    header.extend_from_slice(&pack_u32(self.declared_length));
                    header.extend_from_slice(&pack_u16(ContainerType::Data.to_code()));
                    header.extend_from_slice(&pack_u16(OperationCode::GetObject.into()));
                    header.extend_from_slice(&pack_u32(self.tx_id));
                    Ok(header)
                }
                Phase::Payload { served } => {
                    let take = (self.payload_len - served).min(max_size as u64) as usize;
                    // A read shorter than the request is the short packet that ends the
                    // data phase; a full read means more is coming.
                    let mut data: Vec<u8> = (0..take as u64)
                        .map(|i| synthetic_byte(served + i))
                        .collect();
                    if take < max_size {
                        if self.coalesce_response {
                            data.extend_from_slice(&ok_response(self.tx_id));
                            *phase = Phase::Exhausted;
                        } else {
                            *phase = Phase::Response;
                        }
                    } else {
                        *phase = Phase::Payload {
                            served: served + take as u64,
                        };
                    }
                    Ok(data)
                }
                Phase::Response => {
                    *phase = Phase::Exhausted;
                    Ok(ok_response(self.tx_id))
                }
                Phase::Exhausted => Err(Error::NoDevice),
            }
        }

        async fn receive_interrupt(&self) -> Result<Vec<u8>, Error> {
            Err(Error::NoDevice)
        }

        async fn cancel_transfer(&self, _tx_id: u32, _idle_timeout: Duration) -> Result<(), Error> {
            Ok(())
        }
    }

    /// Read the whole stream, checking every byte and dropping every chunk. Returns
    /// the byte count plus the largest buffer the stream ever held.
    async fn drain_checking_bytes(stream: &mut ReceiveStream) -> (u64, usize) {
        let mut received = 0u64;
        let mut peak_buffer = 0usize;
        while let Some(chunk) = stream.next_chunk().await {
            let chunk = chunk.expect("chunk");
            for (i, byte) in chunk.iter().enumerate() {
                assert_eq!(
                    *byte,
                    synthetic_byte(received + i as u64),
                    "payload mismatch at offset {}",
                    received + i as u64
                );
            }
            received += chunk.len() as u64;
            peak_buffer = peak_buffer.max(stream.buffer.capacity());
        }
        (received, peak_buffer)
    }

    #[tokio::test]
    async fn receive_stream_buffer_stays_bounded_across_a_large_object() {
        // Not a multiple of the read size, so the data phase ends on a short packet.
        const PAYLOAD: u64 = 16 * 1024 * 1024 + 1000;
        let transport =
            BigObjectTransport::serving((HEADER_SIZE as u64 + PAYLOAD) as u32, PAYLOAD, 1);
        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        let (received, peak_buffer) = drain_checking_bytes(&mut stream).await;

        assert_eq!(received, PAYLOAD);
        assert!(
            peak_buffer <= 8 * BULK_READ,
            "buffer grew to {peak_buffer} bytes streaming a {PAYLOAD}-byte object"
        );
    }

    #[tokio::test]
    async fn receive_stream_ends_cleanly_on_the_large_object_length_sentinel() {
        // MTP 1.1 appendix H.1: a data phase over 4 GiB carries 0xFFFFFFFF as its
        // ContainerLength, so the phase ends at the short packet, not at a byte count.
        // Fake the header rather than move 4 GiB of real bytes.
        const PAYLOAD: u64 = 3 * BULK_READ as u64 + 1000;
        let transport = BigObjectTransport::serving(0xFFFF_FFFF, PAYLOAD, 1);
        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        let (received, peak_buffer) = drain_checking_bytes(&mut stream).await;

        assert_eq!(received, PAYLOAD);
        assert!(
            peak_buffer <= 8 * BULK_READ,
            "buffer grew to {peak_buffer} bytes on the sentinel path"
        );
    }

    #[tokio::test]
    async fn receive_stream_sized_ends_a_sentinel_container_on_the_byte_count() {
        // The response container arrives tacked onto the last payload read, so nothing
        // about the packet shape marks the end: only the caller-supplied size does.
        const PAYLOAD: u64 = 2 * BULK_READ as u64 + 1000;
        let transport = BigObjectTransport::coalescing_response(0xFFFF_FFFF, PAYLOAD, 1);
        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session
            .execute_with_receive_stream_sized(OperationCode::GetObject, &[1], PAYLOAD)
            .await
            .unwrap();

        let (received, _) = drain_checking_bytes(&mut stream).await;

        assert_eq!(received, PAYLOAD);
        assert!(stream.done);
    }

    #[tokio::test]
    async fn receive_stream_tolerates_the_zero_length_packet_ending_a_data_phase() {
        // A payload that exactly fills the last read is followed by a zero-length
        // packet, the USB terminator for a data phase that divides the packet size.
        const PAYLOAD: u64 = 4 * BULK_READ as u64;
        let transport =
            BigObjectTransport::serving((HEADER_SIZE as u64 + PAYLOAD) as u32, PAYLOAD, 1);
        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        let (received, peak_buffer) = drain_checking_bytes(&mut stream).await;

        assert_eq!(received, PAYLOAD);
        assert!(
            peak_buffer <= 8 * BULK_READ,
            "buffer grew to {peak_buffer} bytes"
        );
    }

    #[tokio::test]
    async fn test_receive_stream_small_file() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // GetObject data response (small file fits in one container)
        let file_data = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        mock.queue_response(data_container(1, OperationCode::GetObject, &file_data));
        mock.queue_response(ok_response(1));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());

        // Use streaming API
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        // Collect all chunks
        let mut received = Vec::new();
        while let Some(result) = stream.next_chunk().await {
            let chunk = result.unwrap();
            received.extend_from_slice(&chunk);
        }

        assert_eq!(received, file_data);
    }

    #[tokio::test]
    async fn test_receive_stream_collect() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let file_data = vec![1, 2, 3, 4, 5];
        mock.queue_response(data_container(1, OperationCode::GetObject, &file_data));
        mock.queue_response(ok_response(1));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());

        let stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();
        let collected = stream.collect().await.unwrap();

        assert_eq!(collected, file_data);
    }

    #[tokio::test]
    async fn test_receive_stream_error_response() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // Return error response instead of data
        mock.queue_response(response_with_params(
            2,
            ResponseCode::InvalidObjectHandle,
            &[],
        ));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());

        let mut stream = session.get_object_stream(ObjectHandle(999)).await.unwrap();

        // Should get error when reading
        let result = stream.next_chunk().await;
        assert!(result.is_some());
        let err = result.unwrap();
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn test_send_stream_small_file() {
        use futures::stream;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // SendObject response

        let session = PtpSession::open(transport, 1).await.unwrap();

        // Create a small data stream (use iter instead of once for Unpin)
        let data = vec![1u8, 2, 3, 4, 5];
        let data_stream = stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from(data.clone()))]);

        // Send using streaming API
        session.send_object_stream(5, data_stream).await.unwrap();
    }

    #[tokio::test]
    async fn test_send_stream_multiple_chunks() {
        use futures::stream;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // SendObject response

        let session = PtpSession::open(transport, 1).await.unwrap();

        // Create a multi-chunk data stream
        let chunks = vec![
            Ok::<_, std::io::Error>(Bytes::from(vec![1, 2, 3])),
            Ok(Bytes::from(vec![4, 5, 6])),
            Ok(Bytes::from(vec![7, 8, 9, 10])),
        ];
        let data_stream = stream::iter(chunks);

        // Send using streaming API (total size = 10)
        session.send_object_stream(10, data_stream).await.unwrap();
    }

    #[tokio::test]
    async fn test_receive_stream_to_stream_conversion() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let file_data = vec![1, 2, 3, 4, 5];
        mock.queue_response(data_container(1, OperationCode::GetObject, &file_data));
        mock.queue_response(ok_response(1));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());

        let recv_stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        // Convert to futures::Stream and use StreamExt
        // Use pin_mut! to make it Unpin
        use futures::StreamExt;
        use std::pin::pin;
        let mut stream = pin!(receive_stream_to_stream(recv_stream));

        let mut collected = Vec::new();
        while let Some(result) = stream.next().await {
            collected.extend_from_slice(&result.unwrap());
        }

        assert_eq!(collected, file_data);
    }

    #[tokio::test]
    async fn test_cancel_already_done() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let file_data = vec![1, 2, 3];
        mock.queue_response(data_container(1, OperationCode::GetObject, &file_data));
        mock.queue_response(ok_response(1));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        // Consume the entire stream
        while let Some(result) = stream.next_chunk().await {
            result.unwrap();
        }

        // Cancel on a completed stream is a no-op
        stream.cancel(Duration::from_secs(2)).await.unwrap();

        // cancel_transfer should NOT have been called (stream was already done)
        assert!(mock.get_cancel_calls().is_empty());
    }

    #[tokio::test]
    async fn test_cancel_calls_transport_cancel_transfer() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let file_data = vec![1, 2, 3, 4, 5];
        mock.queue_response(data_container(1, OperationCode::GetObject, &file_data));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        // Read one chunk
        stream.next_chunk().await.unwrap().unwrap();

        // Cancel mid-stream, should delegate to transport.cancel_transfer()
        stream.cancel(Duration::from_secs(2)).await.unwrap();

        // Verify cancel_transfer was called with the correct transaction ID
        let cancel_calls = mock.get_cancel_calls();
        assert_eq!(cancel_calls, vec![1]); // tx_id=1 (first operation after OpenSession)
    }

    #[tokio::test]
    async fn test_cancel_propagates_transport_error() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let file_data = vec![1, 2, 3];
        mock.queue_response(data_container(1, OperationCode::GetObject, &file_data));

        // Queue a cancel failure
        mock.queue_cancel_result(Err(crate::PtpError::Disconnected));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        // Read one chunk
        stream.next_chunk().await.unwrap().unwrap();

        // Cancel should propagate the transport error
        let result = stream.cancel(Duration::from_secs(2)).await;
        assert!(result.is_err());

        // Stream should be marked done even on error
        assert!(stream.done);
    }

    /// The streaming download path skips a late reply from an earlier transaction too (#43).
    #[tokio::test]
    async fn receive_stream_skips_a_late_reply_from_an_earlier_transaction() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1));
        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        session
            .execute(OperationCode::GetDeviceInfo, &[])
            .await
            .unwrap(); // tx=1

        mock.queue_response(response_with_params(
            1,
            ResponseCode::TransactionCancelled,
            &[],
        ));
        mock.queue_response(data_container(2, OperationCode::GetObject, &[1, 2, 3]));
        mock.queue_response(ok_response(2));
        let stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();
        assert_eq!(stream.collect().await.unwrap(), vec![1, 2, 3]);
    }

    /// A cancel that fails leaves the rest of the object and its response in the pipe, so
    /// the next operation must drain first. Regression for #43: the POCO X6 Pro STALLs the
    /// CLASS_CANCEL request, and the next op read leftover file bytes as its response.
    #[tokio::test]
    async fn failed_cancel_flags_session_for_recovery() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(data_container(1, OperationCode::GetObject, &[1, 2, 3]));
        mock.queue_cancel_result(Err(crate::PtpError::Io(std::io::Error::other(
            "endpoint stalled",
        ))));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();
        stream.next_chunk().await.unwrap().unwrap();

        assert!(stream.cancel(Duration::from_secs(2)).await.is_err());
        assert!(
            session.needs_recovery(),
            "a failed cancel must flag the session for recovery"
        );
    }

    /// After `DeviceReset` the session is gone and the caller must reopen quietly, so a
    /// recovery drain on the dead session would only add the USB traffic the device can't take.
    #[tokio::test]
    async fn cancel_ending_in_device_reset_does_not_flag_recovery() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(data_container(1, OperationCode::GetObject, &[1, 2, 3]));
        mock.queue_cancel_result(Err(crate::PtpError::DeviceReset));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();
        stream.next_chunk().await.unwrap().unwrap();

        assert!(stream.cancel(Duration::from_secs(2)).await.is_err());
        assert!(!session.needs_recovery());
    }

    #[tokio::test]
    async fn test_cancel_marks_stream_done() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let file_data = vec![1, 2, 3];
        mock.queue_response(data_container(1, OperationCode::GetObject, &file_data));

        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let mut stream = session.get_object_stream(ObjectHandle(1)).await.unwrap();

        // Read one chunk
        stream.next_chunk().await.unwrap().unwrap();

        // Cancel
        stream.cancel(Duration::from_secs(2)).await.unwrap();

        // Stream should be done, next_chunk returns None
        assert!(stream.next_chunk().await.is_none());

        // Second cancel is a no-op (no additional cancel_transfer call)
        stream.cancel(Duration::from_secs(2)).await.unwrap();
        assert_eq!(mock.get_cancel_calls().len(), 1);
    }
}
