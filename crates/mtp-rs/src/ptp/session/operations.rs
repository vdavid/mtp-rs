//! High-level PTP/MTP operations.
//!
//! This module contains methods for common operations like getting device info,
//! listing storage and objects, downloading/uploading files, etc.

use crate::ptp::{
    pack_string, unpack_u32_array, unpack_u64, DeviceInfo, EventContainer, ObjectFormatCode,
    ObjectHandle, ObjectInfo, ObjectPropertyCode, OperationCode, StorageId, StorageInfo,
};
use crate::PtpError as Error;

use super::PtpSession;

impl PtpSession {
    /// Returns information about the device including its capabilities,
    /// manufacturer, model, and supported operations.
    pub async fn get_device_info(&self) -> Result<DeviceInfo, Error> {
        let (response, data) = self
            .execute_with_receive(OperationCode::GetDeviceInfo, &[])
            .await?;
        Self::check_response(&response, OperationCode::GetDeviceInfo)?;
        DeviceInfo::from_bytes(&data)
    }

    /// Returns a list of storage IDs available on the device.
    pub async fn get_storage_ids(&self) -> Result<Vec<StorageId>, Error> {
        let (response, data) = self
            .execute_with_receive(OperationCode::GetStorageIds, &[])
            .await?;
        Self::check_response(&response, OperationCode::GetStorageIds)?;
        let (ids, _) = unpack_u32_array(&data)?;
        Ok(ids.into_iter().map(StorageId).collect())
    }

    /// Returns information about a specific storage, including capacity,
    /// free space, and filesystem type.
    pub async fn get_storage_info(&self, storage_id: StorageId) -> Result<StorageInfo, Error> {
        let (response, data) = self
            .execute_with_receive(OperationCode::GetStorageInfo, &[storage_id.0])
            .await?;
        Self::check_response(&response, OperationCode::GetStorageInfo)?;
        StorageInfo::from_bytes(&data)
    }

    /// Get object handles.
    ///
    /// Returns a list of object handles matching the specified criteria.
    ///
    /// # Arguments
    ///
    /// * `storage_id` - Storage to search, or `StorageId::ALL` for all storages
    /// * `format` - Filter by format, or `None` for all formats
    /// * `parent` - Parent folder handle, or `None` for root level only,
    ///   or `Some(ObjectHandle::ALL)` for recursive listing
    pub async fn get_object_handles(
        &self,
        storage_id: StorageId,
        format: Option<ObjectFormatCode>,
        parent: Option<ObjectHandle>,
    ) -> Result<Vec<ObjectHandle>, Error> {
        let format_code = format.map(|f| u16::from(f) as u32).unwrap_or(0);
        let parent_handle = parent.map(|p| p.0).unwrap_or(0); // 0 = root only

        let (response, data) = self
            .execute_with_receive(
                OperationCode::GetObjectHandles,
                &[storage_id.0, format_code, parent_handle],
            )
            .await?;
        Self::check_response(&response, OperationCode::GetObjectHandles)?;
        let (handles, _) = unpack_u32_array(&data)?;
        Ok(handles.into_iter().map(ObjectHandle).collect())
    }

    /// Returns metadata about an object, including filename, size, and timestamps.
    ///
    /// Note: the `size` field is parsed from the standard `ObjectInfo` dataset, which
    /// uses a u32 for `ObjectCompressedSize` and saturates at `u32::MAX` for files
    /// larger than 4 GB. Use [`get_object_info_full`](Self::get_object_info_full)
    /// to auto-resolve the real u64 size via `GetObjectPropValue`.
    pub async fn get_object_info(&self, handle: ObjectHandle) -> Result<ObjectInfo, Error> {
        let (response, data) = self
            .execute_with_receive(OperationCode::GetObjectInfo, &[handle.0])
            .await?;
        Self::check_response(&response, OperationCode::GetObjectInfo)?;
        ObjectInfo::from_bytes(&data)
    }

    /// Returns object metadata with the full 64-bit size resolved.
    ///
    /// Calls [`get_object_info`](Self::get_object_info) first. If the returned
    /// `size` is saturated at `u32::MAX` (indicating the file is larger than 4 GB),
    /// it follows up with `GetObjectPropValue(ObjectSize)` to retrieve the real
    /// u64 size and overwrites the field before returning.
    ///
    /// Falls back to the saturated value if the device doesn't support
    /// `GetObjectPropValue`, doesn't support the `ObjectSize` property, or if
    /// the response can't be parsed. Transport-level errors from the follow-up
    /// are swallowed so that listing operations aren't aborted by an optional
    /// property lookup; the saturated size is returned instead.
    pub async fn get_object_info_full(&self, handle: ObjectHandle) -> Result<ObjectInfo, Error> {
        let mut info = self.get_object_info(handle).await?;
        if info.size == u64::from(u32::MAX) {
            if let Ok(bytes) = self
                .get_object_prop_value(handle, ObjectPropertyCode::ObjectSize)
                .await
            {
                if let Ok(real_size) = unpack_u64(&bytes) {
                    info.size = real_size;
                }
            }
            // Any error (op not supported, prop not supported, parse failure, transport
            // error) leaves `info.size` at the saturated u32::MAX. Callers that need the
            // exact size for >4 GB objects on devices without GetObjectPropValue support
            // must fetch it through another mechanism.
        }
        Ok(info)
    }

    /// Downloads the complete data of an object.
    pub async fn get_object(&self, handle: ObjectHandle) -> Result<Vec<u8>, Error> {
        let (response, data) = self
            .execute_with_receive(OperationCode::GetObject, &[handle.0])
            .await?;
        Self::check_response(&response, OperationCode::GetObject)?;
        Ok(data)
    }

    /// Get partial object.
    ///
    /// Downloads a portion of an object's data. Uses the standard `GetPartialObject`
    /// operation, which has a 32-bit offset: offsets beyond 4 GB are truncated to
    /// u32 and will silently wrap. For files larger than 4 GB, use
    /// [`get_partial_object_64()`](Self::get_partial_object_64) instead.
    ///
    /// # Arguments
    ///
    /// * `handle` - The object handle
    /// * `offset` - Byte offset to start from (truncated to u32)
    /// * `max_bytes` - Maximum number of bytes to retrieve
    pub async fn get_partial_object(
        &self,
        handle: ObjectHandle,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<u8>, Error> {
        // GetPartialObject params: handle, offset (u32), max_bytes (u32)
        // Note: offset is truncated to u32 in standard MTP
        let (response, data) = self
            .execute_with_receive(
                OperationCode::GetPartialObject,
                &[handle.0, offset as u32, max_bytes],
            )
            .await?;
        Self::check_response(&response, OperationCode::GetPartialObject)?;
        Ok(data)
    }

    /// Get partial object with a 64-bit offset.
    ///
    /// Uses the Android/MTP extension `GetPartialObject64` (`0x95C1`) which supports
    /// offsets beyond 4 GB. Only devices that advertise this op in their
    /// `DeviceInfo::operations_supported` list will accept it; others return
    /// `OperationNotSupported`.
    ///
    /// # Arguments
    ///
    /// * `handle` - The object handle
    /// * `offset` - 64-bit byte offset to start from
    /// * `max_bytes` - Maximum number of bytes to retrieve
    pub async fn get_partial_object_64(
        &self,
        handle: ObjectHandle,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<u8>, Error> {
        // GetPartialObject64 params: handle, offset_lo (u32), offset_hi (u32), max_bytes (u32)
        let offset_lo = offset as u32;
        let offset_hi = (offset >> 32) as u32;
        let (response, data) = self
            .execute_with_receive(
                OperationCode::GetPartialObject64,
                &[handle.0, offset_lo, offset_hi, max_bytes],
            )
            .await?;
        Self::check_response(&response, OperationCode::GetPartialObject64)?;
        Ok(data)
    }

    /// Downloads the thumbnail image for an object.
    pub async fn get_thumb(&self, handle: ObjectHandle) -> Result<Vec<u8>, Error> {
        let (response, data) = self
            .execute_with_receive(OperationCode::GetThumb, &[handle.0])
            .await?;
        Self::check_response(&response, OperationCode::GetThumb)?;
        Ok(data)
    }

    /// Send object info (prepare for upload).
    ///
    /// This must be called before `send_object()` to prepare the device for
    /// receiving a new object.
    ///
    /// # Returns
    ///
    /// Returns a tuple of (storage_id, parent_handle, new_object_handle) where:
    /// - `storage_id` - The storage where the object will be created
    /// - `parent_handle` - The parent folder handle
    /// - `new_object_handle` - The handle assigned to the new object
    pub async fn send_object_info(
        &self,
        storage_id: StorageId,
        parent: ObjectHandle,
        info: &ObjectInfo,
    ) -> Result<(StorageId, ObjectHandle, ObjectHandle), Error> {
        let data = info.to_bytes()?;
        let response = self
            .execute_with_send(
                OperationCode::SendObjectInfo,
                &[storage_id.0, parent.0],
                &data,
            )
            .await?;
        Self::check_response(&response, OperationCode::SendObjectInfo)?;

        // Response params: storage_id, parent_handle, object_handle
        if response.params.len() < 3 {
            return Err(Error::invalid_data(
                "SendObjectInfo response missing params",
            ));
        }
        Ok((
            StorageId(response.params[0]),
            ObjectHandle(response.params[1]),
            ObjectHandle(response.params[2]),
        ))
    }

    /// Send object data (must follow send_object_info).
    ///
    /// Uploads the actual data for an object. This must be called immediately
    /// after `send_object_info()`.
    pub async fn send_object(&self, data: &[u8]) -> Result<(), Error> {
        let response = self
            .execute_with_send(OperationCode::SendObject, &[], data)
            .await?;
        Self::check_response(&response, OperationCode::SendObject)?;
        Ok(())
    }

    /// Deletes an object from the device.
    pub async fn delete_object(&self, handle: ObjectHandle) -> Result<(), Error> {
        // Param2 is format code, 0 means any format
        let response = self
            .execute(OperationCode::DeleteObject, &[handle.0, 0])
            .await?;
        Self::check_response(&response, OperationCode::DeleteObject)?;
        Ok(())
    }

    /// Moves an object to a different location.
    pub async fn move_object(
        &self,
        handle: ObjectHandle,
        storage_id: StorageId,
        parent: ObjectHandle,
    ) -> Result<(), Error> {
        let response = self
            .execute(
                OperationCode::MoveObject,
                &[handle.0, storage_id.0, parent.0],
            )
            .await?;
        Self::check_response(&response, OperationCode::MoveObject)?;
        Ok(())
    }

    /// Copies an object to a new location.
    /// Returns the handle of the newly created copy.
    pub async fn copy_object(
        &self,
        handle: ObjectHandle,
        storage_id: StorageId,
        parent: ObjectHandle,
    ) -> Result<ObjectHandle, Error> {
        let response = self
            .execute(
                OperationCode::CopyObject,
                &[handle.0, storage_id.0, parent.0],
            )
            .await?;
        Self::check_response(&response, OperationCode::CopyObject)?;

        if response.params.is_empty() {
            return Err(Error::invalid_data("CopyObject response missing handle"));
        }
        Ok(ObjectHandle(response.params[0]))
    }

    /// Get object property value.
    ///
    /// Retrieves the value of a specific property for an object.
    /// This is an MTP extension operation (0x9803).
    ///
    /// # Arguments
    ///
    /// * `handle` - The object handle
    /// * `property` - The property code to retrieve
    ///
    /// # Returns
    ///
    /// Returns the raw property value as bytes.
    pub async fn get_object_prop_value(
        &self,
        handle: ObjectHandle,
        property: ObjectPropertyCode,
    ) -> Result<Vec<u8>, Error> {
        let (response, data) = self
            .execute_with_receive(
                OperationCode::GetObjectPropValue,
                &[handle.0, u16::from(property) as u32],
            )
            .await?;
        Self::check_response(&response, OperationCode::GetObjectPropValue)?;
        Ok(data)
    }

    /// Get object property list.
    ///
    /// Retrieves property values of `handle` and, with a nonzero `depth`, of the objects below
    /// it, in one transaction. This is an MTP extension operation (0x9805).
    ///
    /// # Arguments
    ///
    /// * `handle` - The object to start from, or `ObjectHandle::ALL` for every object
    /// * `format` - Only objects of this format, or `None` for all formats
    /// * `property` - One property, or `None` for all of them (`0xFFFFFFFF`). A property code
    ///   of `0` (`ObjectPropertyCode::Unknown(0)`) asks the device to select by `group`
    /// * `group` - The property group code; only read when the property code is `0`
    /// * `depth` - `0` for `handle` alone, `1` to add its direct children, and so on
    ///   (`0xFFFFFFFF` for all levels). A device may decline a depth it doesn't support
    ///
    /// # Returns
    ///
    /// Returns the raw dataset; parse it with [`ObjectPropListElement::list_from_bytes`]. An
    /// answer for `depth > 0` may include `handle` itself, so a caller after the children alone
    /// should filter it out by handle.
    ///
    /// [`ObjectPropListElement::list_from_bytes`]: crate::ptp::ObjectPropListElement::list_from_bytes
    pub async fn get_object_prop_list(
        &self,
        handle: ObjectHandle,
        format: Option<ObjectFormatCode>,
        property: Option<ObjectPropertyCode>,
        group: u32,
        depth: u32,
    ) -> Result<Vec<u8>, Error> {
        let format_code = format.map(|f| u32::from(u16::from(f))).unwrap_or(0);
        let property_code = property
            .map(|p| u32::from(u16::from(p)))
            .unwrap_or(0xFFFF_FFFF);
        let (response, data) = self
            .execute_with_receive(
                OperationCode::GetObjectPropList,
                &[handle.0, format_code, property_code, group, depth],
            )
            .await?;
        Self::check_response(&response, OperationCode::GetObjectPropList)?;
        Ok(data)
    }

    /// Set object property value.
    ///
    /// Sets the value of a specific property for an object.
    /// This is an MTP extension operation (0x9804).
    ///
    /// # Arguments
    ///
    /// * `handle` - The object handle
    /// * `property` - The property code to set
    /// * `value` - The raw property value as bytes
    pub async fn set_object_prop_value(
        &self,
        handle: ObjectHandle,
        property: ObjectPropertyCode,
        value: &[u8],
    ) -> Result<(), Error> {
        let response = self
            .execute_with_send(
                OperationCode::SetObjectPropValue,
                &[handle.0, u16::from(property) as u32],
                value,
            )
            .await?;
        Self::check_response(&response, OperationCode::SetObjectPropValue)?;
        Ok(())
    }

    /// Rename an object (file or folder).
    ///
    /// This is a convenience method that uses SetObjectPropValue to change
    /// the ObjectFileName property (0xDC07).
    ///
    /// # Arguments
    ///
    /// * `handle` - The object handle to rename
    /// * `new_name` - The new filename
    ///
    /// # Note
    ///
    /// Not all devices support renaming. Check `supports_rename()` on DeviceInfo first.
    pub async fn rename_object(&self, handle: ObjectHandle, new_name: &str) -> Result<(), Error> {
        let name_bytes = pack_string(new_name)?;
        self.set_object_prop_value(handle, ObjectPropertyCode::ObjectFileName, &name_bytes)
            .await
    }

    // --- Capture operations ---

    /// Initiate a capture operation.
    ///
    /// This triggers the camera to capture an image. The operation is asynchronous;
    /// use `poll_event()` to wait for `CaptureComplete` and `ObjectAdded` events.
    ///
    /// # Arguments
    ///
    /// * `storage_id` - Target storage (use `StorageId(0)` for camera default)
    /// * `format` - Object format for the capture (use `ObjectFormatCode::Undefined`
    ///   for camera default)
    ///
    /// # Events
    ///
    /// After calling this method, monitor for these events:
    /// - `EventCode::CaptureComplete` - Capture operation finished
    /// - `EventCode::ObjectAdded` - New object (image) was created on device
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::ptp::{EventCode, ObjectFormatCode, PtpDevice, StorageId};
    ///
    /// # async fn example() -> Result<(), mtp_rs::ptp::PtpError> {
    /// # let device = PtpDevice::open_first().await?;
    /// # let session = device.open_session().await?;
    /// // Trigger capture
    /// session.initiate_capture(StorageId(0), ObjectFormatCode::Undefined).await?;
    ///
    /// // Wait for events
    /// loop {
    ///     match session.poll_event().await? {
    ///         Some(event) if event.code == EventCode::CaptureComplete => {
    ///             println!("Capture complete!");
    ///             break;
    ///         }
    ///         Some(event) if event.code == EventCode::ObjectAdded => {
    ///             println!("New object: {}", event.params[0]);
    ///         }
    ///         _ => continue,
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn initiate_capture(
        &self,
        storage_id: StorageId,
        format: ObjectFormatCode,
    ) -> Result<(), Error> {
        // Per PTP spec, 0x00000000 means "any format" / "use device default".
        // ObjectFormatCode::Undefined (0x3000) is different and may not be accepted.
        let format_code = match format {
            ObjectFormatCode::Undefined => 0,
            other => u16::from(other) as u32,
        };
        let response = self
            .execute(OperationCode::InitiateCapture, &[storage_id.0, format_code])
            .await?;
        Self::check_response(&response, OperationCode::InitiateCapture)?;
        Ok(())
    }

    // --- Event handling ---

    /// Poll for a single event from the interrupt endpoint.
    ///
    /// This method awaits **indefinitely** until an event is received from the USB
    /// interrupt endpoint. Events are asynchronous notifications from the device
    /// about changes such as objects being added/removed, storage changes, etc.
    /// Callers should wrap this in an async timeout to avoid blocking forever.
    ///
    /// Note: This method does not require the operation lock since events are
    /// received on the interrupt endpoint, which is independent of bulk transfers.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(container))` - An event was received
    /// - `Ok(None)` - No event (unreachable in practice; kept for API compatibility)
    /// - `Err(_)` - Communication error
    pub async fn poll_event(&self) -> Result<Option<EventContainer>, Error> {
        match self.transport.receive_interrupt().await {
            Ok(bytes) => {
                let container = EventContainer::from_bytes(&bytes)?;
                Ok(Some(container))
            }
            Err(Error::Timeout) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::session::tests::{
        data_container, mock_transport, ok_response, response_with_params,
    };
    use crate::ptp::{
        pack_string, pack_u16, pack_u32, pack_u32_array, ContainerType, ResponseCode,
    };

    fn event_container(code: u16, params: [u32; 3]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(24);
        buf.extend_from_slice(&pack_u32(24)); // length = 24
        buf.extend_from_slice(&pack_u16(ContainerType::Event.to_code()));
        buf.extend_from_slice(&pack_u16(code));
        buf.extend_from_slice(&pack_u32(0)); // transaction_id
        buf.extend_from_slice(&pack_u32(params[0]));
        buf.extend_from_slice(&pack_u32(params[1]));
        buf.extend_from_slice(&pack_u32(params[2]));
        buf
    }

    #[tokio::test]
    async fn test_get_storage_ids() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // GetStorageIds data response
        let storage_ids_data = pack_u32_array(&[0x00010001, 0x00010002]);
        mock.queue_response(data_container(
            1,
            OperationCode::GetStorageIds,
            &storage_ids_data,
        ));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let ids = session.get_storage_ids().await.unwrap();

        assert_eq!(ids, vec![StorageId(0x00010001), StorageId(0x00010002)]);
    }

    #[tokio::test]
    async fn test_get_object_handles() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // GetObjectHandles data response
        let handles_data = pack_u32_array(&[1, 2, 3]);
        mock.queue_response(data_container(
            1,
            OperationCode::GetObjectHandles,
            &handles_data,
        ));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let handles = session
            .get_object_handles(StorageId::ALL, None, None)
            .await
            .unwrap();

        assert_eq!(
            handles,
            vec![ObjectHandle(1), ObjectHandle(2), ObjectHandle(3)]
        );
    }

    #[tokio::test]
    async fn test_get_object() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // GetObject data response
        let object_data = vec![0x01, 0x02, 0x03, 0x04, 0x05];
        mock.queue_response(data_container(1, OperationCode::GetObject, &object_data));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let data = session.get_object(ObjectHandle(1)).await.unwrap();

        assert_eq!(data, object_data);
    }

    #[tokio::test]
    async fn test_delete_object() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // DeleteObject

        let session = PtpSession::open(transport, 1).await.unwrap();
        session.delete_object(ObjectHandle(1)).await.unwrap();
    }

    #[tokio::test]
    async fn test_copy_object() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(response_with_params(1, ResponseCode::Ok, &[100])); // CopyObject with new handle

        let session = PtpSession::open(transport, 1).await.unwrap();
        let new_handle = session
            .copy_object(ObjectHandle(1), StorageId(0x00010001), ObjectHandle::ROOT)
            .await
            .unwrap();

        assert_eq!(new_handle, ObjectHandle(100));
    }

    // --- Event polling tests ---

    #[tokio::test]
    async fn test_poll_event_object_added() {
        use crate::ptp::EventCode;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // Queue an ObjectAdded event (code 0x4002)
        mock.queue_interrupt(event_container(0x4002, [42, 0, 0]));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let event = session.poll_event().await.unwrap().unwrap();

        assert_eq!(event.code, EventCode::ObjectAdded);
        assert_eq!(event.params[0], 42);
    }

    #[tokio::test]
    async fn test_poll_event_store_removed() {
        use crate::ptp::EventCode;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // Queue a StoreRemoved event (code 0x4005)
        mock.queue_interrupt(event_container(0x4005, [0x00010001, 0, 0]));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let event = session.poll_event().await.unwrap().unwrap();

        assert_eq!(event.code, EventCode::StoreRemoved);
        assert_eq!(event.params[0], 0x00010001);
    }

    #[tokio::test]
    async fn test_poll_event_multiple_events() {
        use crate::ptp::EventCode;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // Queue multiple events
        mock.queue_interrupt(event_container(0x4002, [1, 0, 0])); // ObjectAdded
        mock.queue_interrupt(event_container(0x4002, [2, 0, 0])); // ObjectAdded
        mock.queue_interrupt(event_container(0x4003, [1, 0, 0])); // ObjectRemoved

        let session = PtpSession::open(transport, 1).await.unwrap();

        let event1 = session.poll_event().await.unwrap().unwrap();
        assert_eq!(event1.code, EventCode::ObjectAdded);
        assert_eq!(event1.params[0], 1);

        let event2 = session.poll_event().await.unwrap().unwrap();
        assert_eq!(event2.code, EventCode::ObjectAdded);
        assert_eq!(event2.params[0], 2);

        let event3 = session.poll_event().await.unwrap().unwrap();
        assert_eq!(event3.code, EventCode::ObjectRemoved);
        assert_eq!(event3.params[0], 1);
    }

    // --- Object property and rename tests ---

    #[tokio::test]
    async fn test_get_object_prop_value() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // GetObjectPropValue data response (property value is raw bytes)
        let prop_value = vec![0x05, 0x48, 0x00, 0x69, 0x00, 0x00, 0x00]; // Packed string "Hi"
        mock.queue_response(data_container(
            1,
            OperationCode::GetObjectPropValue,
            &prop_value,
        ));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let data = session
            .get_object_prop_value(ObjectHandle(1), ObjectPropertyCode::ObjectFileName)
            .await
            .unwrap();

        assert_eq!(data, prop_value);
    }

    #[tokio::test]
    async fn test_set_object_prop_value() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // SetObjectPropValue

        let session = PtpSession::open(transport, 1).await.unwrap();
        let prop_value = pack_string("newfile.txt").unwrap();
        session
            .set_object_prop_value(
                ObjectHandle(1),
                ObjectPropertyCode::ObjectFileName,
                &prop_value,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_set_object_prop_value_not_supported() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(response_with_params(
            1,
            ResponseCode::OperationNotSupported,
            &[],
        ));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let prop_value = pack_string("newfile.txt").unwrap();
        let result = session
            .set_object_prop_value(
                ObjectHandle(1),
                ObjectPropertyCode::ObjectFileName,
                &prop_value,
            )
            .await;

        assert!(matches!(
            result,
            Err(crate::PtpError::Protocol {
                code: ResponseCode::OperationNotSupported,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_rename_object() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // SetObjectPropValue (for rename)

        let session = PtpSession::open(transport, 1).await.unwrap();
        session
            .rename_object(ObjectHandle(1), "renamed.txt")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rename_object_refuses_a_too_long_name_before_sending_anything() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        let session = PtpSession::open(transport, 1).await.unwrap();
        let sends_after_open = mock.get_sends().len();
        let result = session
            .rename_object(ObjectHandle(1), &"a".repeat(255))
            .await;

        assert!(matches!(
            result,
            Err(crate::PtpError::StringTooLong { units: 255 })
        ));
        assert_eq!(
            mock.get_sends().len(),
            sends_after_open,
            "no command may go out, or the session would wait on a data phase that never comes"
        );
    }

    #[tokio::test]
    async fn test_rename_object_not_supported() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(response_with_params(
            1,
            ResponseCode::OperationNotSupported,
            &[],
        ));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let result = session.rename_object(ObjectHandle(1), "renamed.txt").await;

        assert!(matches!(
            result,
            Err(crate::PtpError::Protocol {
                code: ResponseCode::OperationNotSupported,
                ..
            })
        ));
    }

    // --- Capture tests ---

    #[tokio::test]
    async fn test_initiate_capture() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // InitiateCapture

        let session = PtpSession::open(transport, 1).await.unwrap();
        session
            .initiate_capture(StorageId(0), ObjectFormatCode::Undefined)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_initiate_capture_with_format() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // InitiateCapture

        let session = PtpSession::open(transport, 1).await.unwrap();
        session
            .initiate_capture(StorageId(0x00010001), ObjectFormatCode::Jpeg)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_initiate_capture_not_supported() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(response_with_params(
            1,
            ResponseCode::OperationNotSupported,
            &[],
        ));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let result = session
            .initiate_capture(StorageId(0), ObjectFormatCode::Undefined)
            .await;

        assert!(matches!(
            result,
            Err(crate::PtpError::Protocol {
                code: ResponseCode::OperationNotSupported,
                ..
            })
        ));
    }
}
