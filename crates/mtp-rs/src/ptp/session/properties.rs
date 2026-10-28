//! Device property operations.
//!
//! This module contains methods for getting and setting device properties,
//! primarily used for digital cameras to query/set settings like ISO, aperture, etc.

use crate::ptp::{
    DevicePropDesc, DevicePropertyCode, OperationCode, PropertyDataType, PropertyValue,
};
use crate::PtpError as Error;

use super::PtpSession;

impl PtpSession {
    // =========================================================================
    // Device property operations
    // =========================================================================

    /// Get the descriptor for a device property.
    ///
    /// Returns detailed information about the property including its type,
    /// current value, default value, and allowed values/range.
    ///
    /// This is primarily used for digital cameras to query settings like
    /// ISO, aperture, shutter speed, etc. Most Android MTP devices do not
    /// support device properties.
    ///
    /// # Arguments
    ///
    /// * `property` - The device property code to query
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mtp_rs::ptp::{PtpDevice, DevicePropertyCode};
    ///
    /// # async fn example() -> Result<(), mtp_rs::Error> {
    /// # let device = PtpDevice::open_first().await?;
    /// # let session = device.open_session().await?;
    /// let desc = session.get_device_prop_desc(DevicePropertyCode::BatteryLevel).await?;
    /// println!("Battery level: {:?}", desc.current_value);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_device_prop_desc(
        &self,
        property: DevicePropertyCode,
    ) -> Result<DevicePropDesc, Error> {
        let (response, data) = self
            .execute_with_receive(
                OperationCode::GetDevicePropDesc,
                &[u16::from(property) as u32],
            )
            .await?;
        Self::check_response(&response, OperationCode::GetDevicePropDesc)?;
        DevicePropDesc::from_bytes(&data)
    }

    /// Get the current value of a device property.
    ///
    /// Returns the raw bytes of the property value. To interpret the value,
    /// you need to know the property's data type. Use `get_device_prop_desc()`
    /// to get the full descriptor including the data type.
    ///
    /// # Arguments
    ///
    /// * `property` - The device property code to query
    pub async fn get_device_prop_value(
        &self,
        property: DevicePropertyCode,
    ) -> Result<Vec<u8>, Error> {
        let (response, data) = self
            .execute_with_receive(
                OperationCode::GetDevicePropValue,
                &[u16::from(property) as u32],
            )
            .await?;
        Self::check_response(&response, OperationCode::GetDevicePropValue)?;
        Ok(data)
    }

    /// Get a device property value as a typed PropertyValue.
    ///
    /// This is a convenience method that parses the raw bytes according to
    /// the specified data type.
    ///
    /// # Arguments
    ///
    /// * `property` - The device property code to query
    /// * `data_type` - The expected data type of the property
    pub async fn get_device_prop_value_typed(
        &self,
        property: DevicePropertyCode,
        data_type: PropertyDataType,
    ) -> Result<PropertyValue, Error> {
        let data = self.get_device_prop_value(property).await?;
        let (value, _) = PropertyValue::from_bytes(&data, data_type)?;
        Ok(value)
    }

    /// Set a device property value.
    ///
    /// The value should be the raw bytes of the new value. The value type
    /// must match the property's data type.
    ///
    /// # Arguments
    ///
    /// * `property` - The device property code to set
    /// * `value` - The raw bytes of the new value
    pub async fn set_device_prop_value(
        &self,
        property: DevicePropertyCode,
        value: &[u8],
    ) -> Result<(), Error> {
        let response = self
            .execute_with_send(
                OperationCode::SetDevicePropValue,
                &[u16::from(property) as u32],
                value,
            )
            .await?;
        Self::check_response(&response, OperationCode::SetDevicePropValue)?;
        Ok(())
    }

    /// Set a device property value from a PropertyValue.
    ///
    /// This is a convenience method that serializes the PropertyValue to bytes.
    ///
    /// # Arguments
    ///
    /// * `property` - The device property code to set
    /// * `value` - The new value
    pub async fn set_device_prop_value_typed(
        &self,
        property: DevicePropertyCode,
        value: &PropertyValue,
    ) -> Result<(), Error> {
        let data = value.to_bytes()?;
        self.set_device_prop_value(property, &data).await
    }

    /// Reset a device property to its default value.
    ///
    /// # Arguments
    ///
    /// * `property` - The device property code to reset
    pub async fn reset_device_prop_value(&self, property: DevicePropertyCode) -> Result<(), Error> {
        let response = self
            .execute(
                OperationCode::ResetDevicePropValue,
                &[u16::from(property) as u32],
            )
            .await?;
        Self::check_response(&response, OperationCode::ResetDevicePropValue)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::session::tests::{
        data_container, mock_transport, ok_response, response_with_params,
    };
    use crate::ptp::{pack_u16, ResponseCode};

    /// Build a battery level property descriptor for testing.
    fn build_battery_prop_desc(current: u8) -> Vec<u8> {
        let mut buf = Vec::new();
        // PropertyCode: 0x5001 (BatteryLevel)
        buf.extend_from_slice(&pack_u16(0x5001));
        // DataType: UINT8 (0x0002)
        buf.extend_from_slice(&pack_u16(0x0002));
        // GetSet: read-only (0x00)
        buf.push(0x00);
        // DefaultValue: 100
        buf.push(100);
        // CurrentValue
        buf.push(current);
        // FormFlag: Range (0x01)
        buf.push(0x01);
        // Range: min=0, max=100, step=1
        buf.push(0); // min
        buf.push(100); // max
        buf.push(1); // step
        buf
    }

    #[tokio::test]
    async fn test_get_device_prop_desc() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // Queue battery level prop desc
        let prop_desc_data = build_battery_prop_desc(75);
        mock.queue_response(data_container(
            1,
            OperationCode::GetDevicePropDesc,
            &prop_desc_data,
        ));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let desc = session
            .get_device_prop_desc(DevicePropertyCode::BatteryLevel)
            .await
            .unwrap();

        assert_eq!(desc.property_code, DevicePropertyCode::BatteryLevel);
        assert_eq!(desc.data_type, PropertyDataType::Uint8);
        assert!(!desc.writable);
        assert_eq!(desc.current_value, PropertyValue::Uint8(75));
    }

    #[tokio::test]
    async fn test_get_device_prop_desc_not_supported() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(response_with_params(
            1,
            ResponseCode::DevicePropNotSupported,
            &[],
        ));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let result = session
            .get_device_prop_desc(DevicePropertyCode::BatteryLevel)
            .await;

        assert!(matches!(
            result,
            Err(crate::PtpError::Protocol {
                code: ResponseCode::DevicePropNotSupported,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_get_device_prop_value() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // Queue battery level value (75%)
        let value_data = vec![75u8];
        mock.queue_response(data_container(
            1,
            OperationCode::GetDevicePropValue,
            &value_data,
        ));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let data = session
            .get_device_prop_value(DevicePropertyCode::BatteryLevel)
            .await
            .unwrap();

        assert_eq!(data, vec![75u8]);
    }

    #[tokio::test]
    async fn test_get_device_prop_value_typed() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession

        // Queue ISO value (400 = 0x0190)
        let value_data = vec![0x90, 0x01]; // 400 in little-endian
        mock.queue_response(data_container(
            1,
            OperationCode::GetDevicePropValue,
            &value_data,
        ));
        mock.queue_response(ok_response(1));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let value = session
            .get_device_prop_value_typed(
                DevicePropertyCode::ExposureIndex,
                PropertyDataType::Uint16,
            )
            .await
            .unwrap();

        assert_eq!(value, PropertyValue::Uint16(400));
    }

    #[tokio::test]
    async fn test_set_device_prop_value() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // SetDevicePropValue

        let session = PtpSession::open(transport, 1).await.unwrap();
        let value = vec![0x90, 0x01]; // 400 in little-endian
        session
            .set_device_prop_value(DevicePropertyCode::ExposureIndex, &value)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_set_device_prop_value_typed() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // SetDevicePropValue

        let session = PtpSession::open(transport, 1).await.unwrap();
        session
            .set_device_prop_value_typed(
                DevicePropertyCode::ExposureIndex,
                &PropertyValue::Uint16(400),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_set_device_prop_value_invalid() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(response_with_params(
            1,
            ResponseCode::InvalidDevicePropValue,
            &[],
        ));

        let session = PtpSession::open(transport, 1).await.unwrap();
        let result = session
            .set_device_prop_value(DevicePropertyCode::ExposureIndex, &[0x00, 0x00])
            .await;

        assert!(matches!(
            result,
            Err(crate::PtpError::Protocol {
                code: ResponseCode::InvalidDevicePropValue,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_reset_device_prop_value() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0)); // OpenSession
        mock.queue_response(ok_response(1)); // ResetDevicePropValue

        let session = PtpSession::open(transport, 1).await.unwrap();
        session
            .reset_device_prop_value(DevicePropertyCode::ExposureIndex)
            .await
            .unwrap();
    }
}
