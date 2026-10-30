//! MTP/PTP USB container format.
//!
//! This module implements the USB container format used for MTP/PTP communication.
//! All containers share a common 12-byte header followed by optional parameters or payload.
//!
//! ## Container format (little-endian)
//!
//! Header (12 bytes):
//! - Offset 0: Length (u32) - Total container size including header
//! - Offset 4: Type (u16) - Container type
//! - Offset 6: Code (u16) - Operation/Response/Event code
//! - Offset 8: TransactionID (u32)
//!
//! After header: parameters (each u32) or payload bytes.

use super::{pack_u16, pack_u32, unpack_u16, unpack_u32, EventCode, OperationCode, ResponseCode};

/// Minimum container header size in bytes.
const HEADER_SIZE: usize = 12;

/// Container type identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub(crate) enum ContainerType {
    /// Command container (sent to device).
    Command = 1,
    /// Data container (bidirectional).
    Data = 2,
    /// Response container (from device).
    Response = 3,
    /// Event container (from device).
    Event = 4,
}

impl ContainerType {
    /// Convert a raw u16 value to a ContainerType.
    #[must_use]
    pub(crate) fn from_code(code: u16) -> Option<Self> {
        match code {
            1 => Some(ContainerType::Command),
            2 => Some(ContainerType::Data),
            3 => Some(ContainerType::Response),
            4 => Some(ContainerType::Event),
            _ => None,
        }
    }

    /// Convert a ContainerType to its raw u16 value.
    #[must_use]
    pub(crate) fn to_code(self) -> u16 {
        self as u16
    }
}

/// Determine the container type from a raw buffer.
///
/// Returns an error if the buffer is too small or contains an invalid container type.
pub(crate) fn container_type(buf: &[u8]) -> Result<ContainerType, crate::PtpError> {
    if buf.len() < HEADER_SIZE {
        return Err(crate::PtpError::invalid_data(format!(
            "container too small: need at least {} bytes, have {}",
            HEADER_SIZE,
            buf.len()
        )));
    }

    let type_code = unpack_u16(&buf[4..6])?;
    ContainerType::from_code(type_code).ok_or_else(|| {
        crate::PtpError::invalid_data(format!("invalid container type: {}", type_code))
    })
}

/// Command container sent to the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandContainer {
    /// Operation code for the command.
    pub code: OperationCode,
    /// Transaction ID for this operation.
    pub transaction_id: u32,
    /// Parameters (0-5 u32 values).
    pub params: Vec<u32>,
}

impl CommandContainer {
    /// Serialize the command container to bytes.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let param_bytes = self.params.len() * 4;
        let total_len = HEADER_SIZE + param_bytes;

        let mut buf = Vec::with_capacity(total_len);

        // Header
        buf.extend_from_slice(&pack_u32(total_len as u32));
        buf.extend_from_slice(&pack_u16(ContainerType::Command.to_code()));
        buf.extend_from_slice(&pack_u16(self.code.into()));
        buf.extend_from_slice(&pack_u32(self.transaction_id));

        // Parameters
        for &param in &self.params {
            buf.extend_from_slice(&pack_u32(param));
        }

        buf
    }
}

/// Data container for transferring payload data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DataContainer {
    /// Operation code this data belongs to.
    pub code: OperationCode,
    /// Transaction ID for this operation.
    pub transaction_id: u32,
    /// Payload bytes.
    pub payload: Vec<u8>,
}

impl DataContainer {
    /// Serialize the data container to bytes.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let total_len = HEADER_SIZE + self.payload.len();

        let mut buf = Vec::with_capacity(total_len);

        // Header
        buf.extend_from_slice(&pack_u32(total_len as u32));
        buf.extend_from_slice(&pack_u16(ContainerType::Data.to_code()));
        buf.extend_from_slice(&pack_u16(self.code.into()));
        buf.extend_from_slice(&pack_u32(self.transaction_id));

        // Payload
        buf.extend_from_slice(&self.payload);

        buf
    }

    /// Parse a data container from bytes.
    pub(crate) fn from_bytes(buf: &[u8]) -> Result<Self, crate::PtpError> {
        if buf.len() < HEADER_SIZE {
            return Err(crate::PtpError::invalid_data(format!(
                "data container too small: need at least {} bytes, have {}",
                HEADER_SIZE,
                buf.len()
            )));
        }

        let length = unpack_u32(&buf[0..4])? as usize;
        let type_code = unpack_u16(&buf[4..6])?;
        let code = unpack_u16(&buf[6..8])?;
        let transaction_id = unpack_u32(&buf[8..12])?;

        // Validate container type
        if type_code != ContainerType::Data.to_code() {
            return Err(crate::PtpError::invalid_data(format!(
                "expected Data container type ({}), got {}",
                ContainerType::Data.to_code(),
                type_code
            )));
        }

        // Validate length - must be at least header size and not exceed buffer
        if length < HEADER_SIZE {
            return Err(crate::PtpError::invalid_data(format!(
                "data container length too small: {} < header size {}",
                length, HEADER_SIZE
            )));
        }
        if buf.len() < length {
            return Err(crate::PtpError::invalid_data(format!(
                "data container length mismatch: header says {}, have {}",
                length,
                buf.len()
            )));
        }

        // Extract payload
        let payload = buf[HEADER_SIZE..length].to_vec();

        Ok(DataContainer {
            code: code.into(),
            transaction_id,
            payload,
        })
    }
}

/// Response container from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResponseContainer {
    /// Response code indicating success or failure.
    pub code: ResponseCode,
    /// Transaction ID this response corresponds to.
    pub transaction_id: u32,
    /// Response parameters (0-5 u32 values).
    pub params: Vec<u32>,
}

impl ResponseContainer {
    /// Parse a response container from bytes.
    pub(crate) fn from_bytes(buf: &[u8]) -> Result<Self, crate::PtpError> {
        if buf.len() < HEADER_SIZE {
            return Err(crate::PtpError::invalid_data(format!(
                "response container too small: need at least {} bytes, have {}",
                HEADER_SIZE,
                buf.len()
            )));
        }

        let length = unpack_u32(&buf[0..4])? as usize;
        let type_code = unpack_u16(&buf[4..6])?;
        let code = unpack_u16(&buf[6..8])?;
        let transaction_id = unpack_u32(&buf[8..12])?;

        // Validate container type
        if type_code != ContainerType::Response.to_code() {
            return Err(crate::PtpError::invalid_data(format!(
                "expected Response container type ({}), got {}",
                ContainerType::Response.to_code(),
                type_code
            )));
        }

        // Validate length
        if buf.len() < length {
            return Err(crate::PtpError::invalid_data(format!(
                "response container length mismatch: header says {}, have {}",
                length,
                buf.len()
            )));
        }

        // Parse parameters
        let param_bytes = length - HEADER_SIZE;
        if !param_bytes.is_multiple_of(4) {
            return Err(crate::PtpError::invalid_data(format!(
                "response parameter bytes not aligned: {} bytes",
                param_bytes
            )));
        }

        let param_count = param_bytes / 4;
        let mut params = Vec::with_capacity(param_count);
        for i in 0..param_count {
            let offset = HEADER_SIZE + i * 4;
            params.push(unpack_u32(&buf[offset..])?);
        }

        Ok(ResponseContainer {
            code: code.into(),
            transaction_id,
            params,
        })
    }

    /// Check if the response indicates success (Ok).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.code == ResponseCode::Ok
    }
}

/// Event container from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventContainer {
    /// Event code identifying the event type.
    pub code: EventCode,
    /// Transaction ID (may be 0 for unsolicited events).
    pub transaction_id: u32,
    /// Event parameters (always exactly 3).
    pub params: [u32; 3],
}

impl EventContainer {
    /// Serialize the event container to bytes.
    ///
    /// Produces a 24-byte container: 12-byte header + 3 u32 parameters.
    #[cfg(test)]
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        const EVENT_SIZE: usize = HEADER_SIZE + 12; // 3 params
        let mut buf = Vec::with_capacity(EVENT_SIZE);
        buf.extend_from_slice(&pack_u32(EVENT_SIZE as u32));
        buf.extend_from_slice(&pack_u16(ContainerType::Event.to_code()));
        buf.extend_from_slice(&pack_u16(self.code.into()));
        buf.extend_from_slice(&pack_u32(self.transaction_id));
        for &param in &self.params {
            buf.extend_from_slice(&pack_u32(param));
        }
        buf
    }

    /// Parse an event container from bytes.
    ///
    /// Events can have 0-3 parameters, so valid sizes are 12-24 bytes
    /// (header + 0-3 u32 params). Missing parameters default to 0.
    pub(crate) fn from_bytes(buf: &[u8]) -> Result<Self, crate::PtpError> {
        const MAX_EVENT_SIZE: usize = HEADER_SIZE + 12; // 24 bytes max (3 params)

        if buf.len() < HEADER_SIZE {
            return Err(crate::PtpError::invalid_data(format!(
                "event container too small: need at least {} bytes, have {}",
                HEADER_SIZE,
                buf.len()
            )));
        }

        let length = unpack_u32(&buf[0..4])? as usize;
        let type_code = unpack_u16(&buf[4..6])?;
        let code = unpack_u16(&buf[6..8])?;
        let transaction_id = unpack_u32(&buf[8..12])?;

        // Validate container type
        if type_code != ContainerType::Event.to_code() {
            return Err(crate::PtpError::invalid_data(format!(
                "expected Event container type ({}), got {}",
                ContainerType::Event.to_code(),
                type_code
            )));
        }

        // Validate length: must be between 12 (header only) and 24 (header + 3 params)
        if !(HEADER_SIZE..=MAX_EVENT_SIZE).contains(&length) {
            return Err(crate::PtpError::invalid_data(format!(
                "event container invalid size: expected 12-24, got {}",
                length
            )));
        }

        // Validate parameter alignment (must be multiple of 4 bytes after header)
        let param_bytes = length - HEADER_SIZE;
        if !param_bytes.is_multiple_of(4) {
            return Err(crate::PtpError::invalid_data(format!(
                "event parameter bytes not aligned: {} bytes",
                param_bytes
            )));
        }

        // Validate buffer has enough data
        if buf.len() < length {
            return Err(crate::PtpError::invalid_data(format!(
                "event container buffer too small: need {}, have {}",
                length,
                buf.len()
            )));
        }

        // Parse parameters (0-3), defaulting missing ones to 0
        let param_count = param_bytes / 4;
        let param1 = if param_count >= 1 {
            unpack_u32(&buf[12..16])?
        } else {
            0
        };
        let param2 = if param_count >= 2 {
            unpack_u32(&buf[16..20])?
        } else {
            0
        };
        let param3 = if param_count >= 3 {
            unpack_u32(&buf[20..24])?
        } else {
            0
        };

        Ok(EventContainer {
            code: code.into(),
            transaction_id,
            params: [param1, param2, param3],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- ContainerType tests ---

    #[test]
    fn container_type_conversions() {
        for (code, ct) in [
            (1, ContainerType::Command),
            (2, ContainerType::Data),
            (3, ContainerType::Response),
            (4, ContainerType::Event),
        ] {
            assert_eq!(ContainerType::from_code(code), Some(ct));
            assert_eq!(ct.to_code(), code);
        }
        for invalid in [0, 5, 0xFFFF] {
            assert_eq!(ContainerType::from_code(invalid), None);
        }
    }

    #[test]
    fn container_type_detection() {
        // Build minimal containers and verify type detection
        let containers: [(u16, ContainerType); 4] = [
            (1, ContainerType::Command),
            (2, ContainerType::Data),
            (3, ContainerType::Response),
            (4, ContainerType::Event),
        ];
        for (type_code, expected) in containers {
            let mut bytes = vec![0x0C, 0x00, 0x00, 0x00]; // length = 12
            bytes.extend_from_slice(&type_code.to_le_bytes());
            bytes.extend_from_slice(&[0x00; 6]); // code + tx_id
            assert_eq!(container_type(&bytes).unwrap(), expected);
        }

        // Invalid type codes
        for invalid in [0u16, 5] {
            let mut bytes = vec![0x0C, 0x00, 0x00, 0x00];
            bytes.extend_from_slice(&invalid.to_le_bytes());
            bytes.extend_from_slice(&[0x00; 6]);
            assert!(container_type(&bytes).is_err());
        }

        // Insufficient bytes
        assert!(container_type(&[]).is_err());
        assert!(container_type(&[0x00; 11]).is_err());
    }

    // --- CommandContainer tests ---

    #[test]
    fn command_container_serialization() {
        let cmd = CommandContainer {
            code: OperationCode::GetObjectHandles,
            transaction_id: 10,
            params: vec![0x00010001, 0x00000000, 0xFFFFFFFF],
        };
        let bytes = cmd.to_bytes();
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[0..4], &[0x18, 0x00, 0x00, 0x00]); // length = 24
        assert_eq!(&bytes[4..6], &[0x01, 0x00]); // type = Command
        assert_eq!(&bytes[6..8], &[0x07, 0x10]); // code = 0x1007
        assert_eq!(&bytes[8..12], &[0x0A, 0x00, 0x00, 0x00]); // tx_id = 10
        assert_eq!(&bytes[12..16], &[0x01, 0x00, 0x01, 0x00]); // param1
    }

    // --- DataContainer tests ---

    #[test]
    fn data_container_roundtrip() {
        let original = DataContainer {
            code: OperationCode::GetObject,
            transaction_id: 100,
            payload: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        };
        let parsed = DataContainer::from_bytes(&original.to_bytes()).unwrap();
        assert_eq!(parsed, original);

        // Empty payload
        let empty = DataContainer {
            code: OperationCode::SendObject,
            transaction_id: 5,
            payload: vec![],
        };
        assert_eq!(DataContainer::from_bytes(&empty.to_bytes()).unwrap(), empty);
    }

    #[test]
    fn data_container_errors() {
        assert!(DataContainer::from_bytes(&[0x00; 11]).is_err()); // Too small

        // Wrong type
        let mut bad_type = vec![0x0C, 0x00, 0x00, 0x00, 0x03, 0x00]; // Response type
        bad_type.extend_from_slice(&[0x00; 6]);
        assert!(DataContainer::from_bytes(&bad_type).is_err());

        // Length > buffer
        let mut truncated = vec![0x20, 0x00, 0x00, 0x00, 0x02, 0x00]; // claims 32 bytes
        truncated.extend_from_slice(&[0x00; 6]);
        assert!(DataContainer::from_bytes(&truncated).is_err());
    }

    // --- ResponseContainer tests ---

    #[test]
    fn response_container_parsing() {
        // OK response with params
        let bytes = [
            0x18, 0x00, 0x00, 0x00, // length = 24
            0x03, 0x00, // type = Response
            0x01, 0x20, // code = OK
            0x02, 0x00, 0x00, 0x00, // tx_id = 2
            0x01, 0x00, 0x01, 0x00, // param1
            0x00, 0x00, 0x00, 0x00, // param2
            0x05, 0x00, 0x00, 0x00, // param3
        ];
        let resp = ResponseContainer::from_bytes(&bytes).unwrap();
        assert_eq!(resp.code, ResponseCode::Ok);
        assert!(resp.is_ok());
        assert_eq!(resp.params, vec![0x00010001, 0, 5]);

        // Error response
        let err_bytes = [
            0x0C, 0x00, 0x00, 0x00, 0x03, 0x00, 0x02, 0x20, // GeneralError
            0x03, 0x00, 0x00, 0x00,
        ];
        let err_resp = ResponseContainer::from_bytes(&err_bytes).unwrap();
        assert_eq!(err_resp.code, ResponseCode::GeneralError);
        assert!(!err_resp.is_ok());
    }

    #[test]
    fn response_container_errors() {
        assert!(ResponseContainer::from_bytes(&[0x00; 11]).is_err());

        // Unaligned params (13 bytes = 12 header + 1)
        let unaligned = [
            0x0D, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x20, 0x01, 0x00, 0x00, 0x00, 0xFF,
        ];
        assert!(ResponseContainer::from_bytes(&unaligned).is_err());
    }

    // --- EventContainer tests ---

    #[test]
    fn event_container_to_bytes_roundtrip() {
        let original = EventContainer {
            code: EventCode::CancelTransaction,
            transaction_id: 42,
            params: [42, 0, 0],
        };
        let bytes = original.to_bytes();
        assert_eq!(bytes.len(), 24);
        let parsed = EventContainer::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn event_container_variable_params() {
        // 0 params (12 bytes)
        let zero = [
            0x0C, 0x00, 0x00, 0x00, 0x04, 0x00, 0x08, 0x40, 0x00, 0x00, 0x00, 0x00,
        ];
        let e0 = EventContainer::from_bytes(&zero).unwrap();
        assert_eq!(e0.code, EventCode::DeviceInfoChanged);
        assert_eq!(e0.params, [0, 0, 0]);

        // 1 param (16 bytes) - common on Android
        let one = [
            0x10, 0x00, 0x00, 0x00, 0x04, 0x00, 0x02, 0x40, 0x00, 0x00, 0x00, 0x00, 0x2A, 0x00,
            0x00, 0x00,
        ];
        let e1 = EventContainer::from_bytes(&one).unwrap();
        assert_eq!(e1.params, [42, 0, 0]);

        // 3 params (24 bytes)
        let three = [
            0x18, 0x00, 0x00, 0x00, 0x04, 0x00, 0x02, 0x40, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00,
        ];
        let e3 = EventContainer::from_bytes(&three).unwrap();
        assert_eq!(e3.transaction_id, 10);
        assert_eq!(e3.params, [1, 2, 3]);
    }

    #[test]
    fn event_container_errors() {
        assert!(EventContainer::from_bytes(&[0x00; 11]).is_err());

        // Length > 24 (too many params)
        let too_long = [
            0x1C, 0x00, 0x00, 0x00, 0x04, 0x00, 0x02, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(EventContainer::from_bytes(&too_long).is_err());

        // Unaligned (14 bytes)
        let unaligned = [
            0x0E, 0x00, 0x00, 0x00, 0x04, 0x00, 0x02, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(EventContainer::from_bytes(&unaligned).is_err());
    }

    // --- Property-based tests ---

    fn valid_response_bytes(param_count: usize) -> impl Strategy<Value = Vec<u8>> {
        (
            any::<u16>(),
            any::<u32>(),
            prop::collection::vec(any::<u32>(), param_count..=param_count),
        )
            .prop_map(move |(code, tx_id, params)| {
                let len = HEADER_SIZE + params.len() * 4;
                let mut bytes = Vec::with_capacity(len);
                bytes.extend_from_slice(&pack_u32(len as u32));
                bytes.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
                bytes.extend_from_slice(&pack_u16(code));
                bytes.extend_from_slice(&pack_u32(tx_id));
                for p in &params {
                    bytes.extend_from_slice(&pack_u32(*p));
                }
                bytes
            })
    }

    proptest! {
        #[test]
        fn prop_container_type_roundtrip(code in 1u16..=4u16) {
            let ct = ContainerType::from_code(code).unwrap();
            prop_assert_eq!(ct.to_code(), code);
        }

        #[test]
        fn prop_data_container_roundtrip(
            code in any::<u16>(),
            tx_id in any::<u32>(),
            payload in prop::collection::vec(any::<u8>(), 0..500)
        ) {
            let original = DataContainer {
                code: code.into(),
                transaction_id: tx_id,
                payload: payload.clone(),
            };
            let parsed = DataContainer::from_bytes(&original.to_bytes()).unwrap();
            prop_assert_eq!(parsed, original);
        }

        #[test]
        fn prop_command_container_length(
            code in any::<u16>(),
            tx_id in any::<u32>(),
            params in prop::collection::vec(any::<u32>(), 0..5)
        ) {
            let cmd = CommandContainer {
                code: code.into(),
                transaction_id: tx_id,
                params: params.clone(),
            };
            let bytes = cmd.to_bytes();
            let length = unpack_u32(&bytes[0..4]).unwrap() as usize;
            prop_assert_eq!(length, HEADER_SIZE + params.len() * 4);
            prop_assert_eq!(length, bytes.len());
        }

        #[test]
        fn prop_response_container_parse(param_count in 0usize..=5usize) {
            let strategy = valid_response_bytes(param_count);
            proptest!(|(bytes in strategy)| {
                let resp = ResponseContainer::from_bytes(&bytes).unwrap();
                prop_assert_eq!(resp.params.len(), param_count);
            });
        }

        #[test]
        fn prop_container_type_identification(
            code in any::<u16>(),
            tx_id in any::<u32>(),
            payload in prop::collection::vec(any::<u8>(), 0..50)
        ) {
            let data = DataContainer {
                code: code.into(),
                transaction_id: tx_id,
                payload,
            };
            prop_assert_eq!(container_type(&data.to_bytes()).unwrap(), ContainerType::Data);

            let cmd = CommandContainer {
                code: code.into(),
                transaction_id: tx_id,
                params: vec![],
            };
            prop_assert_eq!(container_type(&cmd.to_bytes()).unwrap(), ContainerType::Command);
        }

        // Adversarial tests

        #[test]
        fn fuzz_data_container_length_underflow(fake_length in 0u32..12u32, tx_id: u32) {
            let mut buf = fake_length.to_le_bytes().to_vec();
            buf.extend_from_slice(&2u16.to_le_bytes());
            buf.extend_from_slice(&0x1001u16.to_le_bytes());
            buf.extend_from_slice(&tx_id.to_le_bytes());
            prop_assert!(DataContainer::from_bytes(&buf).is_err());
        }

        #[test]
        fn fuzz_event_container_invalid_length(
            fake_length in prop::sample::select(vec![
                0u32, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, // Too small
                13, 14, 15, 17, 18, 19, 21, 22, 23,      // Unaligned
                25, 26, 28, 32, 100,                      // Too large
            ]),
            tx_id: u32,
        ) {
            let mut buf = fake_length.to_le_bytes().to_vec();
            buf.extend_from_slice(&4u16.to_le_bytes());
            buf.extend_from_slice(&0x4002u16.to_le_bytes());
            buf.extend_from_slice(&tx_id.to_le_bytes());
            buf.extend_from_slice(&[0u8; 12]); // 3 params
            prop_assert!(EventContainer::from_bytes(&buf).is_err());
        }

        #[test]
        fn fuzz_wrong_container_type(
            tx_id: u32,
            payload in prop::collection::vec(any::<u8>(), 0..20),
        ) {
            let len = 12 + payload.len();
            for (parser_type, wrong_type) in [(2u16, 1u16), (2, 3), (2, 4), (3, 1), (3, 2), (4, 1)] {
                let mut buf = (len as u32).to_le_bytes().to_vec();
                buf.extend_from_slice(&wrong_type.to_le_bytes());
                buf.extend_from_slice(&0x1001u16.to_le_bytes());
                buf.extend_from_slice(&tx_id.to_le_bytes());
                buf.extend_from_slice(&payload);

                match parser_type {
                    2 => prop_assert!(DataContainer::from_bytes(&buf).is_err()),
                    3 => prop_assert!(ResponseContainer::from_bytes(&buf).is_err()),
                    4 => prop_assert!(EventContainer::from_bytes(&buf).is_err()),
                    _ => {}
                }
            }
        }
    }

    // Fuzz tests - verify parsers don't panic on arbitrary input
    crate::fuzz_bytes_fn!(fuzz_container_type, container_type, 100);
    crate::fuzz_bytes!(fuzz_data_container, DataContainer, 100);
    crate::fuzz_bytes!(fuzz_response_container, ResponseContainer, 100);
    crate::fuzz_bytes!(fuzz_event_container, EventContainer, 100);
}
