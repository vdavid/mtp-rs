//! USB transport abstraction layer.

#[cfg(test)]
pub mod mock;
pub mod nusb;

#[cfg(feature = "virtual-device")]
pub mod virtual_device;

pub use self::nusb::{MtpMatchReason, NusbTransport, UsbDeviceInfo, UsbSpeed};

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::time::Duration;

/// A boxed stream of byte chunks used by [`Transport::send_bulk_streaming`].
pub type BulkStream<'a> = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'a>>;

/// Transport trait for MTP/PTP communication.
///
/// Abstracts USB communication to enable testing with mock transport.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send data on the bulk OUT endpoint.
    async fn send_bulk(&self, data: &[u8]) -> Result<(), crate::PtpError>;

    /// Send data as a continuous bulk transfer from a stream of chunks.
    ///
    /// All chunks are sent as one logical USB transfer, properly terminated
    /// with a short packet or ZLP. This avoids buffering the entire payload
    /// in memory.
    ///
    /// The default implementation collects all chunks and calls `send_bulk`.
    /// USB transports should override this to use native streaming writes.
    async fn send_bulk_streaming(&self, chunks: BulkStream<'_>) -> Result<(), crate::PtpError> {
        use futures::StreamExt;
        let mut buffer = Vec::new();
        let mut stream = chunks;
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(crate::PtpError::Io)?;
            buffer.extend_from_slice(&chunk);
        }
        self.send_bulk(&buffer).await
    }

    /// Receive data from the bulk IN endpoint.
    ///
    /// `max_size` is the maximum bytes to receive in one call.
    async fn receive_bulk(&self, max_size: usize) -> Result<Vec<u8>, crate::PtpError>;

    /// Receive event data from the interrupt IN endpoint.
    ///
    /// This may block until an event is available.
    async fn receive_interrupt(&self) -> Result<Vec<u8>, crate::PtpError>;

    /// Cancel an in-progress transfer using the USB Still Image Class mechanism.
    ///
    /// Sends a CLASS_CANCEL control request (`bRequest=0x64`), then drains
    /// remaining data from the bulk IN and interrupt pipes. After this call,
    /// the session is clean and ready for the next operation.
    ///
    /// `idle_timeout` controls how long to wait during drain steps before
    /// assuming each pipe is clear. 300ms (matching libmtp / Windows) is
    /// the recommended default; see [`mtp::DEFAULT_CANCEL_TIMEOUT`](crate::mtp::DEFAULT_CANCEL_TIMEOUT).
    async fn cancel_transfer(
        &self,
        transaction_id: u32,
        idle_timeout: Duration,
    ) -> Result<(), crate::PtpError>;

    /// Read and discard whatever the device has queued on the bulk IN pipe, until it stays
    /// quiet for `idle_timeout` or `max_duration` passes. Returns the bytes discarded.
    ///
    /// Unlike [`cancel_transfer`](Self::cancel_transfer), this sends nothing to the device.
    /// [`PtpSession::open`](crate::ptp::PtpSession::open) uses it when the reply to
    /// `OpenSession` turns out to be leftover data from a host process that died mid-transfer.
    ///
    /// The default implementation is a no-op for transports with no USB pipe.
    async fn drain_bulk_in(
        &self,
        _idle_timeout: Duration,
        _max_duration: Duration,
    ) -> Result<usize, crate::PtpError> {
        Ok(0)
    }

    /// Reset the device to its idle state using the USB Still Image Class
    /// Device Reset request (`bRequest=0x66`), then clear stale transport
    /// state (halted endpoints, leftover bulk IN data).
    ///
    /// This is the recovery tool for a device whose PTP state machine is
    /// stuck: a host process died mid-transfer and leftover data poisons the
    /// next session ("Transaction ID mismatch", "expected Response container
    /// type (3), got 2"), or the device stopped answering after an
    /// interrupted listing. No PTP session is needed.
    ///
    /// The default implementation is a no-op success for transports that
    /// have no USB-level state (mock, virtual device).
    async fn reset_device(&self) -> Result<(), crate::PtpError> {
        Ok(())
    }
}
