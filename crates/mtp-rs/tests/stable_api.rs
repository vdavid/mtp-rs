//! Pins what a consumer can do with the stable API from outside the crate, with no features on.
//!
//! `#[non_exhaustive]` only bites across a crate boundary, so in-crate unit tests can't notice a
//! caller-built type that lost its way to be built. If a change here is needed to make this
//! compile, it's a breaking change for consumers too.

use mtp_rs::ptp::{self, PtpDevice, PtpError};
use mtp_rs::{
    Backend, ByteRange, CancelToken, DateTime, DeviceEvent, Error, ListingItem, MtpDevice,
    MtpDeviceInfo, MtpMatchReason, NewObjectInfo, ObjectFormat, ObjectHandle, ObjectInfo,
    StorageId, UsbSpeed, UtcOffset,
};

#[test]
fn new_object_info_is_buildable_through_its_constructors() {
    let date = DateTime::new(2026, 10, 30, 12, 0, 0).expect("a real date");

    let file = NewObjectInfo::file("photo.jpg", 1024).with_modified(date);
    assert_eq!(file.modified, Some(date));

    let folder = NewObjectInfo::folder("Pictures");
    assert_eq!(folder.format, Some(ObjectFormat::ASSOCIATION));

    let raw = NewObjectInfo::with_format("data.bin", 1, ObjectFormat::UNDEFINED);
    assert_eq!(raw.size, 1);
}

#[test]
fn date_time_is_buildable_and_settable() {
    let utc = DateTime::from_unix_seconds(1_791_376_200, UtcOffset::UTC).expect("in range");
    assert_eq!(utc.offset, Some(UtcOffset::UTC));

    let mut date = DateTime::new(2026, 10, 30, 12, 0, 0).expect("a real date");
    date.offset = UtcOffset::from_minutes(120);
    assert_eq!(date.to_unix_seconds(), Some(1_793_354_400));
}

#[test]
fn ptp_object_info_is_buildable_from_its_default() {
    // `PtpSession::send_object_info` takes one of these, so camera users have to be able to make it.
    let mut info = ptp::ObjectInfo::default();
    info.filename = "photo.jpg".into();
    info.size = 1024;
    assert!(info.is_file());
}

#[test]
fn id_newtypes_and_input_enums_are_buildable() {
    assert_eq!(ObjectHandle(7).0, 7);
    assert_eq!(StorageId(7).0, 7);
    assert_eq!(ptp::ObjectHandle(7).0, 7);

    let ranges = [
        ByteRange::Full,
        ByteRange::From(1),
        ByteRange::Range { offset: 1, len: 2 },
    ];
    assert_eq!(ranges.len(), 3);

    let _ = MtpDevice::builder().backend(Backend::Usb);
    assert!(!CancelToken::new().is_cancelled());
}

#[test]
fn output_types_are_readable_through_root_paths() {
    let info = ObjectInfo::default();
    assert!(info.filename.is_empty());

    let item = ListingItem::Object(info);
    assert!(item.object().is_some());

    // Exhaustive-looking matches need the wildcard: new variants aren't a break.
    let _describe_event = |event: DeviceEvent| match event {
        DeviceEvent::DeviceReset => "reset",
        _ => "other",
    };
    let _describe_speed = |speed: UsbSpeed| match speed {
        UsbSpeed::High => "USB 2",
        _ => "other",
    };
    assert_eq!(MtpMatchReason::KnownVidPid.as_str(), "known_vid_pid");
}

/// Both layers enumerate into the same type, then open by what it reports, with no raw types.
#[test]
fn enumerate_then_open_needs_no_raw_types() {
    fn pick(devices: &[MtpDeviceInfo]) -> Option<u64> {
        devices.first().map(|d| d.location_id)
    }
    assert!(MtpDevice::list_devices().is_ok());
    let devices = PtpDevice::list_devices().expect("enumerating works without a device");
    let _ = pick(&devices);

    let _mtp_open = |location_id: u64| MtpDevice::builder().open_by_location(location_id);
    let _ptp_open = |location_id: u64| PtpDevice::open_by_location(location_id);
}

#[test]
fn a_low_level_error_converts_to_the_neutral_one() {
    let neutral: Error = PtpError::Timeout.into();
    assert!(matches!(neutral, Error::Timeout));
}
