# PTP camera features

This document describes the PTP device property and capture extensions implemented in mtp-rs.

## Overview

The Picture Transfer Protocol (PTP) provides operations for:

- Reading and writing device properties (settings)
- Initiating image capture on cameras

These features are primarily useful for digital cameras, though some MTP devices (like Android phones) may support a
subset of these operations.

Every example here uses only the stable `ptp::` API, with no features on.

## Opening a camera

List USB devices with `PtpDevice::list_devices()`, open the one you want, and start a session:

```rust
use mtp_rs::ptp::{PtpDevice, PtpError as Error, PtpSession};

async fn open_camera() -> Result<PtpSession, Error> {
    let devices = PtpDevice::list_devices()?;
    let camera = devices.first().ok_or(Error::NoDevice)?;
    println!("Opening {}", camera.display());

    let device = PtpDevice::open_by_location(camera.location_id).await?;
    device.open_session().await
}
```

`PtpDevice::open_first()` and `open_by_serial()` skip the listing. Each open also has a `_with_timeout` form.

The examples below assume these imports:

```rust
use mtp_rs::ptp::{
    DevicePropertyCode, EventCode, ObjectFormatCode, ObjectHandle, OperationCode, PropertyDataType,
    PropertyValue, PtpDevice, PtpError as Error, PtpSession, ResponseCode, StorageId,
};
use std::time::Duration;
```

## Device properties

Device properties represent configurable settings on the device, such as:

- Battery level (read-only)
- Date/time
- Image size
- Exposure settings (ISO, aperture, shutter speed)
- White balance
- Focus mode

### Property types

Properties can be one of several data types:

| Type     | Description             | Rust type |
|----------|-------------------------|-----------|
| `Uint8`  | Unsigned 8-bit integer  | `u8`      |
| `Int8`   | Signed 8-bit integer    | `i8`      |
| `Uint16` | Unsigned 16-bit integer | `u16`     |
| `Int16`  | Signed 16-bit integer   | `i16`     |
| `Uint32` | Unsigned 32-bit integer | `u32`     |
| `Int32`  | Signed 32-bit integer   | `i32`     |
| `Uint64` | Unsigned 64-bit integer | `u64`     |
| `Int64`  | Signed 64-bit integer   | `i64`     |
| `String` | UTF-16LE string         | `String`  |

### Property form types

Properties can have constraints on their values:

- **None**: No constraint, any value of the correct type is accepted
- **Range**: Value must be within min/max bounds with a specified step
- **Enumeration**: Value must be one of a predefined set of values

### Reading property descriptors

Use `get_device_prop_desc()` to get full information about a property:

```rust
async fn read_battery(session: &PtpSession) -> Result<(), Error> {
    let desc = session.get_device_prop_desc(DevicePropertyCode::BatteryLevel).await?;

    println!("Battery level property:");
    println!("  Type: {:?}", desc.data_type);
    println!("  Writable: {}", desc.writable);
    println!("  Current value: {:?}", desc.current_value);
    println!("  Default value: {:?}", desc.default_value);

    if let Some(range) = desc.range {
        println!("  Range: {:?} to {:?}", range.min, range.max);
    }

    Ok(())
}
```

### Reading property values

For simple value reads, use `get_device_prop_value_typed()`:

```rust
async fn get_battery_level(session: &PtpSession) -> Result<u8, Error> {
    let value = session
        .get_device_prop_value_typed(
            DevicePropertyCode::BatteryLevel,
            PropertyDataType::Uint8,
        )
        .await?;

    match value {
        PropertyValue::Uint8(level) => Ok(level),
        _ => Err(Error::invalid_data("unexpected type")),
    }
}
```

### Setting property values

Use `set_device_prop_value_typed()` to change a property:

```rust
async fn set_iso(session: &PtpSession, iso: u16) -> Result<(), Error> {
    let value = PropertyValue::Uint16(iso);
    session
        .set_device_prop_value_typed(DevicePropertyCode::ExposureIndex, &value)
        .await
}
```

### Resetting properties

Reset a property to its default value:

```rust
async fn reset_exposure_bias(session: &PtpSession) -> Result<(), Error> {
    session
        .reset_device_prop_value(DevicePropertyCode::ExposureBiasCompensation)
        .await
}
```

## Standard device properties

The following standard device properties are defined in the PTP specification:

| Property                   | Code   | Type   | Description                        |
|----------------------------|--------|--------|------------------------------------|
| `BatteryLevel`             | 0x5001 | Uint8  | Current battery percentage (0-100) |
| `FunctionalMode`           | 0x5002 | Uint16 | Device operating mode              |
| `ImageSize`                | 0x5003 | String | Image resolution setting           |
| `CompressionSetting`       | 0x5004 | Uint8  | Image compression level            |
| `WhiteBalance`             | 0x5005 | Uint16 | White balance mode                 |
| `RgbGain`                  | 0x5006 | String | RGB gain values                    |
| `FNumber`                  | 0x5007 | Uint16 | Aperture (f-stop * 100)            |
| `FocalLength`              | 0x5008 | Uint32 | Current focal length (mm * 100)    |
| `FocusDistance`            | 0x5009 | Uint16 | Focus distance                     |
| `FocusMode`                | 0x500A | Uint16 | Auto/manual focus mode             |
| `ExposureMeteringMode`     | 0x500B | Uint16 | Metering mode                      |
| `FlashMode`                | 0x500C | Uint16 | Flash mode                         |
| `ExposureTime`             | 0x500D | Uint32 | Shutter speed (1/10000 sec)        |
| `ExposureProgramMode`      | 0x500E | Uint16 | Program/Av/Tv/M mode               |
| `ExposureIndex`            | 0x500F | Uint16 | ISO sensitivity                    |
| `ExposureBiasCompensation` | 0x5010 | Int16  | Exposure compensation              |
| `DateTime`                 | 0x5011 | String | Current date/time                  |
| `CaptureDelay`             | 0x5012 | Uint32 | Self-timer delay (ms)              |
| `StillCaptureMode`         | 0x5013 | Uint16 | Single/burst/timer                 |
| `Contrast`                 | 0x5014 | Uint8  | Contrast adjustment                |
| `Sharpness`                | 0x5015 | Uint8  | Sharpness adjustment               |
| `DigitalZoom`              | 0x5016 | Uint8  | Digital zoom level                 |
| `EffectMode`               | 0x5017 | Uint16 | Special effect mode                |
| `BurstNumber`              | 0x5018 | Uint16 | Burst shot count                   |
| `BurstInterval`            | 0x5019 | Uint16 | Burst interval (ms)                |
| `TimelapseNumber`          | 0x501A | Uint16 | Timelapse count                    |
| `TimelapseInterval`        | 0x501B | Uint32 | Timelapse interval (ms)            |
| `FocusMeteringMode`        | 0x501C | Uint16 | Focus point selection              |
| `UploadUrl`                | 0x501D | String | Upload destination URL             |
| `Artist`                   | 0x501E | String | Artist/creator name                |
| `CopyrightInfo`            | 0x501F | String | Copyright string                   |

## Capture operations

### Initiating capture

Use `initiate_capture()` to trigger the camera shutter:

```rust
async fn take_photo(session: &PtpSession) -> Result<(), Error> {
    // Use StorageId(0) for camera default, ObjectFormatCode::Undefined for default format
    session
        .initiate_capture(StorageId(0), ObjectFormatCode::Undefined)
        .await
}
```

### Capture events

After initiating capture, the camera will send events:

- `ObjectAdded` - A new image file was created
- `CaptureComplete` - The capture operation finished

You can poll for events using `poll_event()`:

```rust
async fn capture_and_get_handle(session: &PtpSession) -> Result<ObjectHandle, Error> {
    session.initiate_capture(StorageId(0), ObjectFormatCode::Undefined).await?;

    loop {
        if let Some(event) = session.poll_event().await? {
            match event.code {
                EventCode::ObjectAdded => {
                    let handle = ObjectHandle(event.params[0]);
                    return Ok(handle);
                }
                EventCode::CaptureComplete => {
                    return Err(Error::invalid_data("capture completed without new object"));
                }
                _ => continue,
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
```

## Error handling

Property operations may fail with specific error codes:

| Response code             | Meaning                                         |
|---------------------------|-------------------------------------------------|
| `DevicePropNotSupported`  | The device does not support this property       |
| `InvalidDevicePropFormat` | The value format is incorrect for this property |
| `InvalidDevicePropValue`  | The value is out of range or not allowed        |
| `OperationNotSupported`   | The operation is not supported by this device   |

Example error handling:

```rust
match session.get_device_prop_desc(DevicePropertyCode::ExposureTime).await {
    Ok(desc) => println!("Exposure time: {:?}", desc.current_value),
    Err(Error::Protocol { code: ResponseCode::DevicePropNotSupported, .. }) => {
        println!("Device does not support exposure time property");
    }
    Err(e) => return Err(e),
}
```

## Checking device capabilities

Before using property or capture operations, check if the device supports them:

```rust
async fn check_capabilities(device: &PtpDevice) -> Result<(), Error> {
    let info = device.get_device_info().await?;

    if info.supports_operation(OperationCode::GetDevicePropDesc) {
        println!("Device supports reading property descriptors");
    }

    if info.supports_operation(OperationCode::SetDevicePropValue) {
        println!("Device supports setting property values");
    }

    if info.supports_operation(OperationCode::InitiateCapture) {
        println!("Device supports capture operations");
    }

    Ok(())
}
```

## Device compatibility notes

### Android devices

Android devices in MTP mode typically do not support device properties or capture operations. These features are
primarily for digital cameras.

### Digital cameras

Most digital cameras that support PTP will expose:

- Battery level (read-only)
- Date/time (read/write)
- Various exposure and image settings

The specific properties available depend on the camera model and manufacturer.

### Vendor extensions

Many cameras implement vendor-specific property codes in the 0xD000-0xDFFF range. These are represented as
`DevicePropertyCode::Unknown(code)` and can still be accessed if you know the property code and data type.

Vendor operations work the same way: `PtpSession::execute`, `execute_with_receive`, and `execute_with_send` take any
`OperationCode`, including `OperationCode::Unknown(code)`. The code enums are `#[non_exhaustive]`, so match them with a
wildcard arm: a code that's `Unknown` today can get its own variant in a later release.

### Raw transports

If you enumerate with `nusb` yourself or run PTP over your own transport, the `unstable-raw` feature adds
`PtpSession::open` and `mtp_rs::transport`. It's exempt from SemVer, and nothing above needs it.
