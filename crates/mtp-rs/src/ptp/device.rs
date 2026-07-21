//! Low-level PTP device API.

use crate::ptp::{
    container_type, unpack_u32, CommandContainer, ContainerType, DataContainer, DeviceInfo,
    OperationCode, PtpSession, ResponseCode, ResponseContainer,
};
use crate::transport::{NusbTransport, Transport};
use crate::PtpError as Error;
use std::sync::Arc;
use std::time::Duration;

/// A low-level PTP device connection.
///
/// Use this for camera support or when you need raw PTP operations.
/// For typical MTP usage with Android devices, prefer `MtpDevice` instead.
pub struct PtpDevice {
    transport: Arc<dyn Transport>,
}

impl PtpDevice {
    /// Open a PTP device at a specific USB location (port).
    pub async fn open_by_location(location_id: u64) -> Result<Self, Error> {
        Self::open_by_location_with_timeout(location_id, NusbTransport::DEFAULT_TIMEOUT).await
    }

    /// Open by location with custom timeout.
    pub async fn open_by_location_with_timeout(
        location_id: u64,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let devices = NusbTransport::list_mtp_devices()?;
        let device_info = devices
            .into_iter()
            .find(|d| d.location_id == location_id)
            .ok_or(Error::NoDevice)?;
        Self::open_device(device_info, timeout).await
    }

    /// Open a PTP device by its serial number.
    pub async fn open_by_serial(serial: &str) -> Result<Self, Error> {
        Self::open_by_serial_with_timeout(serial, NusbTransport::DEFAULT_TIMEOUT).await
    }

    /// Open by serial with custom timeout.
    pub async fn open_by_serial_with_timeout(
        serial: &str,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let devices = NusbTransport::list_mtp_devices()?;
        let device_info = devices
            .into_iter()
            .find(|d| d.serial_number.as_deref() == Some(serial))
            .ok_or(Error::NoDevice)?;
        Self::open_device(device_info, timeout).await
    }

    /// Open the first available PTP device.
    pub async fn open_first() -> Result<Self, Error> {
        Self::open_first_with_timeout(NusbTransport::DEFAULT_TIMEOUT).await
    }

    /// Open the first available device with custom timeout.
    pub async fn open_first_with_timeout(timeout: Duration) -> Result<Self, Error> {
        let devices = NusbTransport::list_mtp_devices()?;
        let device_info = devices.into_iter().next().ok_or(Error::NoDevice)?;
        Self::open_device(device_info, timeout).await
    }

    async fn open_device(
        device_info: crate::transport::UsbDeviceInfo,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let device = device_info.open().map_err(Error::Usb)?;
        let transport = NusbTransport::open_with_timeout(device, timeout).await?;
        Ok(Self {
            transport: Arc::new(transport) as Arc<dyn Transport>,
        })
    }

    /// Reset the device's USB transport state without opening a session.
    ///
    /// Sends the USB Still Image Class Device Reset request (`bRequest=0x66`),
    /// clears halted bulk endpoints, and drains stale bulk data. Use this to
    /// recover a device whose PTP state machine is stuck, for example after
    /// a host process died mid-transfer, when every operation fails with
    /// "Transaction ID mismatch" or "expected Response container type"
    /// errors.
    ///
    /// This is the USB transport-level reset, not the in-session
    /// `ResetDevice` (0x1010) PTP operation: it works precisely when the
    /// device is too confused for PTP traffic.
    ///
    /// # Warning: a last resort on Android
    ///
    /// Sending this to a *healthy* Pixel 9 Pro XL permanently killed its MTP
    /// function: Android's `MtpServer` lost its endpoint read (`ECANCELED`, then
    /// `EPIPE`) and never re-armed, while the USB device controller stayed
    /// `configured`, so the phone kept enumerating and answered nothing. Only a
    /// physical replug brought it back (verified on a Pixel 9 Pro XL, with
    /// macOS/nusb and `adb logcat`, 2026-07-21). Reach for it only after spaced
    /// reopens have failed, or on a device that's already unreachable. See
    /// `docs/notes/android-wedges-and-the-reset-kill-switch.md`.
    pub async fn reset_device(&self) -> Result<(), Error> {
        self.transport.reset_device().await
    }

    /// Get device info without opening a session.
    ///
    /// This is the only operation that can be performed without a session.
    pub async fn get_device_info(&self) -> Result<DeviceInfo, Error> {
        // Build GetDeviceInfo command (transaction ID 0 for session-less)
        let cmd = CommandContainer {
            code: OperationCode::GetDeviceInfo,
            transaction_id: 0,
            params: vec![],
        };
        self.transport.send_bulk(&cmd.to_bytes()).await?;

        // Receive data
        // A single PTP container may span multiple USB bulk transfers when the
        // device sends the 12-byte header in one transfer and the payload in
        // follow-up transfers (allowed by the PTP spec, observed on Garmin
        // Forerunner 955). Accumulate transfers until the full container is in
        // hand, matching the in-session receive logic.
        let mut data_payload = Vec::new();
        loop {
            let mut bytes = self.transport.receive_bulk(64 * 1024).await?;
            if bytes.is_empty() {
                return Err(Error::invalid_data("Empty response"));
            }

            let ct = container_type(&bytes)?;
            match ct {
                ContainerType::Data => {
                    if bytes.len() >= 4 {
                        let total_length = unpack_u32(&bytes[0..4])? as usize;
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
                    data_payload.extend_from_slice(&container.payload);
                }
                ContainerType::Response => {
                    let response = ResponseContainer::from_bytes(&bytes)?;
                    if response.code != ResponseCode::Ok {
                        return Err(Error::Protocol {
                            code: response.code,
                            operation: OperationCode::GetDeviceInfo,
                        });
                    }
                    break;
                }
                _ => {
                    return Err(Error::invalid_data(format!(
                        "Unexpected container type: {:?}",
                        ct
                    )));
                }
            }
        }

        DeviceInfo::from_bytes(&data_payload)
    }

    /// Open a PTP session.
    ///
    /// Most operations require a session to be open first.
    pub async fn open_session(&self) -> Result<PtpSession, Error> {
        self.open_session_with_id(1).await
    }

    /// Open a session with a specific session ID.
    pub async fn open_session_with_id(&self, session_id: u32) -> Result<PtpSession, Error> {
        PtpSession::open(Arc::clone(&self.transport), session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::pack::{pack_u16, pack_u16_array, pack_u32};
    use crate::transport::mock::MockTransport;

    #[tokio::test]
    #[ignore] // Requires real device
    async fn test_open_first() {
        let device = PtpDevice::open_first().await.unwrap();
        let info = device.get_device_info().await.unwrap();
        println!("Model: {}", info.model);
    }

    #[tokio::test]
    #[ignore] // Requires real device
    async fn test_open_session() {
        let device = PtpDevice::open_first().await.unwrap();
        let session = device.open_session().await.unwrap();

        let info = session.get_device_info().await.unwrap();
        println!("Model: {}", info.model);

        session.close().await.unwrap();
    }

    /// Build a minimal valid DeviceInfo payload (no header).
    fn minimal_device_info_payload() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&pack_u16(100)); // StandardVersion
        buf.extend_from_slice(&pack_u32(0)); // VendorExtensionID
        buf.extend_from_slice(&pack_u16(0)); // VendorExtensionVersion
        buf.push(0x00); // VendorExtensionDesc (empty string)
        buf.extend_from_slice(&pack_u16(0)); // FunctionalMode
        buf.extend_from_slice(&pack_u16_array(&[])); // OperationsSupported
        buf.extend_from_slice(&pack_u16_array(&[])); // EventsSupported
        buf.extend_from_slice(&pack_u16_array(&[])); // DevicePropertiesSupported
        buf.extend_from_slice(&pack_u16_array(&[])); // CaptureFormats
        buf.extend_from_slice(&pack_u16_array(&[])); // PlaybackFormats
        buf.push(0x00); // Manufacturer (empty)
        buf.push(0x00); // Model (empty)
        buf.push(0x00); // DeviceVersion (empty)
        buf.push(0x00); // SerialNumber (empty)
        buf
    }

    /// Build a 12-byte data container header for GetDeviceInfo.
    fn data_container_header(total_length: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&pack_u32(total_length));
        buf.extend_from_slice(&pack_u16(ContainerType::Data.to_code()));
        buf.extend_from_slice(&pack_u16(OperationCode::GetDeviceInfo.into()));
        buf.extend_from_slice(&pack_u32(0)); // tx_id 0 (session-less)
        buf
    }

    /// Build a 12-byte OK response for GetDeviceInfo (tx_id 0).
    fn ok_response_session_less() -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&pack_u32(12));
        buf.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
        buf.extend_from_slice(&pack_u16(ResponseCode::Ok.into()));
        buf.extend_from_slice(&pack_u32(0));
        buf
    }

    #[tokio::test]
    async fn get_device_info_handles_split_header_and_data() {
        // Regression test: some devices (Garmin Forerunner 955, observed) send
        // the 12-byte container header in one bulk transfer and the payload in
        // a follow-up transfer. The session-less GetDeviceInfo path must
        // accumulate transfers until the full container is received.
        let mock = Arc::new(MockTransport::new());

        let payload = minimal_device_info_payload();
        let total_length = 12 + payload.len() as u32;

        // Split: header in one transfer, payload in another.
        mock.queue_response(data_container_header(total_length));
        mock.queue_response(payload);
        mock.queue_response(ok_response_session_less());

        let device = PtpDevice {
            transport: mock as Arc<dyn Transport>,
        };

        let info = device.get_device_info().await.unwrap();
        assert_eq!(info.standard_version, 100);
    }

    #[tokio::test]
    async fn get_device_info_handles_combined_header_and_data() {
        // Happy path: device sends header and payload as a single bulk transfer.
        let mock = Arc::new(MockTransport::new());

        let payload = minimal_device_info_payload();
        let total_length = 12 + payload.len() as u32;

        let mut combined = data_container_header(total_length);
        combined.extend_from_slice(&payload);
        mock.queue_response(combined);
        mock.queue_response(ok_response_session_less());

        let device = PtpDevice {
            transport: mock as Arc<dyn Transport>,
        };

        let info = device.get_device_info().await.unwrap();
        assert_eq!(info.standard_version, 100);
    }
}
