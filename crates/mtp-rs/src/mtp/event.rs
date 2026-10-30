//! Device events.

use crate::mtp::{ObjectHandle, StorageId};
use crate::ptp::{EventCode, EventContainer};

/// Events from an MTP device.
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    /// A new object was added.
    ObjectAdded {
        /// Handle of the new object.
        handle: ObjectHandle,
    },

    /// An object was removed.
    ObjectRemoved {
        /// Handle of the removed object.
        handle: ObjectHandle,
    },

    /// A storage was added (e.g., SD card inserted).
    StoreAdded {
        /// ID of the new storage.
        storage_id: StorageId,
    },

    /// A storage was removed.
    StoreRemoved {
        /// ID of the removed storage.
        storage_id: StorageId,
    },

    /// Storage info changed (e.g., free space).
    StorageInfoChanged {
        /// ID of the storage that changed.
        storage_id: StorageId,
    },

    /// Object info changed.
    ObjectInfoChanged {
        /// Handle of the object that changed.
        handle: ObjectHandle,
    },

    /// Device info changed.
    DeviceInfoChanged,

    /// Device is being reset.
    DeviceReset,

    /// Unknown event.
    Unknown {
        /// Raw event code.
        code: u16,
        /// Event parameters.
        params: [u32; 3],
    },
}

impl DeviceEvent {
    /// Parse from an event container.
    #[must_use]
    pub(crate) fn from_container(container: &EventContainer) -> Self {
        match container.code {
            EventCode::ObjectAdded => DeviceEvent::ObjectAdded {
                handle: ObjectHandle(u64::from(container.params[0])),
            },
            EventCode::ObjectRemoved => DeviceEvent::ObjectRemoved {
                handle: ObjectHandle(u64::from(container.params[0])),
            },
            EventCode::StoreAdded => DeviceEvent::StoreAdded {
                storage_id: StorageId(u64::from(container.params[0])),
            },
            EventCode::StoreRemoved => DeviceEvent::StoreRemoved {
                storage_id: StorageId(u64::from(container.params[0])),
            },
            EventCode::StorageInfoChanged => DeviceEvent::StorageInfoChanged {
                storage_id: StorageId(u64::from(container.params[0])),
            },
            EventCode::ObjectInfoChanged => DeviceEvent::ObjectInfoChanged {
                handle: ObjectHandle(u64::from(container.params[0])),
            },
            EventCode::DeviceInfoChanged => DeviceEvent::DeviceInfoChanged,
            // All other codes (including Unknown and unhandled known codes like DevicePropChanged)
            other => DeviceEvent::Unknown {
                code: other.into(),
                params: container.params,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_parsing() {
        // Events with object handle param
        for (code, expected_handle) in [
            (EventCode::ObjectAdded, 42),
            (EventCode::ObjectRemoved, 123),
            (EventCode::ObjectInfoChanged, 99),
        ] {
            let container = EventContainer {
                code,
                transaction_id: 0,
                params: [expected_handle, 0, 0],
            };
            let event = DeviceEvent::from_container(&container);
            let handle = match event {
                DeviceEvent::ObjectAdded { handle } => handle,
                DeviceEvent::ObjectRemoved { handle } => handle,
                DeviceEvent::ObjectInfoChanged { handle } => handle,
                _ => panic!("Unexpected event type"),
            };
            assert_eq!(handle, ObjectHandle(u64::from(expected_handle)));
        }

        // Events with storage ID param
        for (code, expected_id) in [
            (EventCode::StoreAdded, 0x00010001),
            (EventCode::StoreRemoved, 0x00010002),
            (EventCode::StorageInfoChanged, 0x00010001),
        ] {
            let container = EventContainer {
                code,
                transaction_id: 0,
                params: [expected_id, 0, 0],
            };
            let event = DeviceEvent::from_container(&container);
            let storage_id = match event {
                DeviceEvent::StoreAdded { storage_id } => storage_id,
                DeviceEvent::StoreRemoved { storage_id } => storage_id,
                DeviceEvent::StorageInfoChanged { storage_id } => storage_id,
                _ => panic!("Unexpected event type"),
            };
            assert_eq!(storage_id, StorageId(u64::from(expected_id)));
        }

        // DeviceInfoChanged (no params)
        let container = EventContainer {
            code: EventCode::DeviceInfoChanged,
            transaction_id: 0,
            params: [0, 0, 0],
        };
        assert!(matches!(
            DeviceEvent::from_container(&container),
            DeviceEvent::DeviceInfoChanged
        ));
    }

    #[test]
    fn unknown_events() {
        // Explicit Unknown code
        let container = EventContainer {
            code: EventCode::Unknown(0x9999),
            transaction_id: 0,
            params: [1, 2, 3],
        };
        match DeviceEvent::from_container(&container) {
            DeviceEvent::Unknown { code, params } => {
                assert_eq!(code, 0x9999);
                assert_eq!(params, [1, 2, 3]);
            }
            _ => panic!("Expected Unknown event"),
        }

        // Known EventCode without DeviceEvent variant (DevicePropChanged)
        let container = EventContainer {
            code: EventCode::DevicePropChanged,
            transaction_id: 0,
            params: [100, 0, 0],
        };
        match DeviceEvent::from_container(&container) {
            DeviceEvent::Unknown { code, params } => {
                assert_eq!(code, 0x4006);
                assert_eq!(params[0], 100);
            }
            _ => panic!("Expected Unknown event"),
        }
    }
}
