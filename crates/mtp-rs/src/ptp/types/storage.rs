//! Storage-related type enums for MTP/PTP.
//!
//! This module contains enums for describing storage characteristics:
//! - [`StorageType`]: Type of storage medium (ROM, RAM, etc.)
//! - [`FilesystemType`]: Type of filesystem on the storage
//! - [`AccessCapability`]: Read/write access capabilities
//! - [`ProtectionStatus`]: Object protection status
//! - [`AssociationType`]: Association type for objects (folder/container type)

use num_enum::{FromPrimitive, IntoPrimitive};

/// Type of storage medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum StorageType {
    /// Undefined storage type.
    Undefined = 0,
    /// Fixed ROM (e.g., internal flash).
    FixedRom = 1,
    /// Removable ROM.
    RemovableRom = 2,
    /// Fixed RAM.
    FixedRam = 3,
    /// Removable RAM (e.g., SD card).
    RemovableRam = 4,
    /// Unknown storage type code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

#[allow(clippy::derivable_impls)] // Can't derive due to num_enum catch_all
impl Default for StorageType {
    fn default() -> Self {
        Self::Undefined
    }
}

/// Type of filesystem on the storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum FilesystemType {
    /// Undefined filesystem type.
    Undefined = 0,
    /// Generic flat filesystem (no folders).
    GenericFlat = 1,
    /// Generic hierarchical filesystem (with folders).
    GenericHierarchical = 2,
    /// DCF (Design rule for Camera File system).
    Dcf = 3,
    /// Unknown filesystem type code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

#[allow(clippy::derivable_impls)] // Can't derive due to num_enum catch_all
impl Default for FilesystemType {
    fn default() -> Self {
        Self::Undefined
    }
}

/// Access capability of the storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum AccessCapability {
    /// Read-write access.
    ReadWrite = 0,
    /// Read-only, deletion not allowed.
    ReadOnlyWithoutDeletion = 1,
    /// Read-only, deletion allowed.
    ReadOnlyWithDeletion = 2,
    /// Unknown access capability code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

#[allow(clippy::derivable_impls)] // Can't derive due to num_enum catch_all
impl Default for AccessCapability {
    fn default() -> Self {
        Self::ReadWrite
    }
}

/// Protection status of an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum ProtectionStatus {
    /// No protection.
    None = 0,
    /// Read-only protection.
    ReadOnly = 1,
    /// Unknown protection status code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

#[allow(clippy::derivable_impls)] // Can't derive due to num_enum catch_all
impl Default for ProtectionStatus {
    fn default() -> Self {
        Self::None
    }
}

/// Association type for objects (folder/container type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum AssociationType {
    /// No association (regular file).
    None = 0,
    /// Generic folder.
    GenericFolder = 1,
    /// Unknown association type code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

#[allow(clippy::derivable_impls)] // Can't derive due to num_enum catch_all
impl Default for AssociationType {
    fn default() -> Self {
        Self::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn storage_type_conversions() {
        for (code, expected) in [
            (0, StorageType::Undefined),
            (1, StorageType::FixedRom),
            (2, StorageType::RemovableRom),
            (3, StorageType::FixedRam),
            (4, StorageType::RemovableRam),
        ] {
            assert_eq!(StorageType::from(code), expected);
            assert_eq!(u16::from(expected), code);
        }
        assert_eq!(StorageType::from(99u16), StorageType::Unknown(99));
        assert_eq!(StorageType::default(), StorageType::Undefined);
    }

    #[test]
    fn filesystem_type_conversions() {
        for (code, expected) in [
            (0, FilesystemType::Undefined),
            (1, FilesystemType::GenericFlat),
            (2, FilesystemType::GenericHierarchical),
            (3, FilesystemType::Dcf),
        ] {
            assert_eq!(FilesystemType::from(code), expected);
            assert_eq!(u16::from(expected), code);
        }
        assert_eq!(FilesystemType::from(99u16), FilesystemType::Unknown(99));
        assert_eq!(FilesystemType::default(), FilesystemType::Undefined);
    }

    #[test]
    fn access_capability_conversions() {
        for (code, expected) in [
            (0, AccessCapability::ReadWrite),
            (1, AccessCapability::ReadOnlyWithoutDeletion),
            (2, AccessCapability::ReadOnlyWithDeletion),
        ] {
            assert_eq!(AccessCapability::from(code), expected);
            assert_eq!(u16::from(expected), code);
        }
        assert_eq!(AccessCapability::from(99u16), AccessCapability::Unknown(99));
        assert_eq!(AccessCapability::default(), AccessCapability::ReadWrite);
    }

    #[test]
    fn protection_status_conversions() {
        for (code, expected) in [(0, ProtectionStatus::None), (1, ProtectionStatus::ReadOnly)] {
            assert_eq!(ProtectionStatus::from(code), expected);
            assert_eq!(u16::from(expected), code);
        }
        assert_eq!(ProtectionStatus::from(99u16), ProtectionStatus::Unknown(99));
        assert_eq!(ProtectionStatus::default(), ProtectionStatus::None);
    }

    #[test]
    fn association_type_conversions() {
        for (code, expected) in [
            (0, AssociationType::None),
            (1, AssociationType::GenericFolder),
        ] {
            assert_eq!(AssociationType::from(code), expected);
            assert_eq!(u16::from(expected), code);
        }
        assert_eq!(AssociationType::from(99u16), AssociationType::Unknown(99));
        assert_eq!(AssociationType::default(), AssociationType::None);
    }

    proptest! {
        #[test]
        fn prop_storage_type_roundtrip(code: u16) {
            let st = StorageType::from(code);
            prop_assert_eq!(u16::from(st), code);
            if code > 4 {
                prop_assert_eq!(st, StorageType::Unknown(code));
            }
        }

        #[test]
        fn prop_filesystem_type_roundtrip(code: u16) {
            let ft = FilesystemType::from(code);
            prop_assert_eq!(u16::from(ft), code);
            if code > 3 {
                prop_assert_eq!(ft, FilesystemType::Unknown(code));
            }
        }

        #[test]
        fn prop_access_capability_roundtrip(code: u16) {
            let ac = AccessCapability::from(code);
            prop_assert_eq!(u16::from(ac), code);
            if code > 2 {
                prop_assert_eq!(ac, AccessCapability::Unknown(code));
            }
        }

        #[test]
        fn prop_protection_status_roundtrip(code: u16) {
            let ps = ProtectionStatus::from(code);
            prop_assert_eq!(u16::from(ps), code);
            if code > 1 {
                prop_assert_eq!(ps, ProtectionStatus::Unknown(code));
            }
        }

        #[test]
        fn prop_association_type_roundtrip(code: u16) {
            let at = AssociationType::from(code);
            prop_assert_eq!(u16::from(at), code);
            if code > 1 {
                prop_assert_eq!(at, AssociationType::Unknown(code));
            }
        }
    }
}
