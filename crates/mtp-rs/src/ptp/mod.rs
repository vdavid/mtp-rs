//! Low-level PTP (Picture Transfer Protocol) implementation.
//!
//! This module provides direct access to the PTP/MTP protocol layer. Use this module when:
//!
//! - Working with digital cameras that use PTP
//! - You need fine-grained control over protocol operations
//! - Implementing custom MTP extensions or vendor operations
//! - You need access to raw response codes for error handling
//! - Building your own high-level abstractions
//!
//! ## When to use `mtp` instead
//!
//! Most users working with Android devices should prefer the high-level [`crate::mtp`] module,
//! which provides a simpler API for common operations like listing files, uploading, and
//! downloading.
//!
//! ## Module structure
//!
//! - `codes`: Operation, response, event, and format code enums
//! - `pack`: Binary serialization/deserialization primitives
//! - `container`: USB container format for PTP messages
//! - `types`: DeviceInfo, StorageInfo, ObjectInfo structures
//! - `session`: PTP session management
//! - `device`: PtpDevice public API
//!
//! ## Example
//!
//! ```rust,no_run
//! use mtp_rs::ptp::PtpDevice;
//!
//! # async fn example() -> Result<(), mtp_rs::Error> {
//! // Open device and start a session
//! let device = PtpDevice::open_first().await?;
//! let session = device.open_session().await?;
//!
//! // Get device info
//! let info = session.get_device_info().await?;
//! println!("Model: {}", info.model);
//!
//! // List storage IDs
//! let storage_ids = session.get_storage_ids().await?;
//! # Ok(())
//! # }
//! ```

mod codes;
mod container;
mod device;
mod pack;
mod session;
#[cfg(test)]
mod test_utils;
mod types;

pub use codes::{
    DevicePropertyCode, EventCode, ObjectFormatCode, ObjectPropertyCode, OperationCode,
    PropertyDataType, ResponseCode,
};
pub use container::{
    container_type, CommandContainer, ContainerType, DataContainer, EventContainer,
    ResponseContainer,
};
pub use device::PtpDevice;
pub use pack::{
    pack_datetime, pack_i16, pack_i32, pack_i64, pack_i8, pack_string, pack_u16, pack_u16_array,
    pack_u32, pack_u32_array, pack_u64, pack_u8, unpack_datetime, unpack_i16, unpack_i32,
    unpack_i64, unpack_i8, unpack_string, unpack_u16, unpack_u16_array, unpack_u32,
    unpack_u32_array, unpack_u64, unpack_u8, DateTime,
};
pub use session::{receive_stream_to_stream, PtpSession, ReceiveStream};
pub use types::{
    AccessCapability, AssociationType, DeviceInfo, DevicePropDesc, FilesystemType, ObjectInfo,
    PropertyFormType, PropertyRange, PropertyValue, ProtectionStatus, StorageInfo, StorageType,
};

/// 32-bit object handle assigned by the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ObjectHandle(pub u32);

impl ObjectHandle {
    /// Root folder (parent = root means object is in storage root).
    pub const ROOT: Self = ObjectHandle(0x00000000);
    /// All objects (used in GetObjectHandles to list recursively).
    pub const ALL: Self = ObjectHandle(0xFFFFFFFF);
    /// Storage root as the *destination* of a new object: `SendObjectInfo`
    /// parameter 2.
    ///
    /// MTP 1.1 D.2.12 is explicit: "If the initiator wishes to place an object
    /// in the root of a given storage, it shall indicate the desired storage in
    /// the first parameter and include a value of 0xFFFFFFFF in the second
    /// parameter." A `0` there means "no parent specified, responder may
    /// choose", so responders look it up as a handle and reject it: Android's
    /// `MtpServer` only maps `MTP_PARENT_ROOT` (0xFFFFFFFF) to the storage path,
    /// and libhaze (Nintendo Switch homebrew, #21) only maps 0xFFFFFFFF to its
    /// storage object. Both answer `0` with `InvalidObjectHandle`.
    ///
    /// The spec is asymmetric, so don't unify this with [`ObjectHandle::ROOT`]:
    /// `MoveObject`/`CopyObject` (D.2.25/D.2.26) spell the same destination
    /// `0x00000000`.
    pub const SEND_ROOT: Self = ObjectHandle(0xFFFFFFFF);
}

/// 32-bit storage identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct StorageId(pub u32);

impl StorageId {
    /// All storages (used in GetObjectHandles to search all).
    pub const ALL: Self = StorageId(0xFFFFFFFF);
}

/// 32-bit session identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SessionId(pub u32);

/// 32-bit transaction identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TransactionId(pub u32);

impl TransactionId {
    /// The first valid transaction ID in a session.
    pub const FIRST: Self = TransactionId(0x00000001);

    /// Invalid transaction ID (must never be used).
    pub const INVALID: Self = TransactionId(0xFFFFFFFF);

    /// Transaction ID for session-less operations (e.g., GetDeviceInfo before OpenSession).
    pub const SESSION_LESS: Self = TransactionId(0x00000000);

    /// Get the next transaction ID, wrapping correctly.
    ///
    /// Wraps from 0xFFFFFFFE to 0x00000001 (skipping both 0x00000000 and 0xFFFFFFFF).
    #[must_use]
    pub fn next(self) -> Self {
        let next = self.0.wrapping_add(1);
        if next == 0 || next == 0xFFFFFFFF {
            TransactionId(0x00000001)
        } else {
            TransactionId(next)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_id_next() {
        assert_eq!(TransactionId(1).next(), TransactionId(2));
        assert_eq!(TransactionId(100).next(), TransactionId(101));
    }

    #[test]
    fn transaction_id_wrapping() {
        // Should wrap from 0xFFFFFFFE to 0x00000001, skipping 0xFFFFFFFF and 0x00000000
        assert_eq!(TransactionId(0xFFFFFFFE).next(), TransactionId(1));
        assert_eq!(TransactionId(0xFFFFFFFD).next(), TransactionId(0xFFFFFFFE));
    }

    #[test]
    fn object_handle_constants() {
        assert_eq!(ObjectHandle::ROOT.0, 0);
        assert_eq!(ObjectHandle::ALL.0, 0xFFFFFFFF);
    }

    #[test]
    fn storage_id_constants() {
        assert_eq!(StorageId::ALL.0, 0xFFFFFFFF);
    }
}
