//! Output shapes shared across subcommands: the row structs that show up in
//! multiple commands and the JSON / progress writers.

use mtp_rs::mtp::{MtpDeviceInfo, Storage};
use mtp_rs::ObjectInfo;
use serde::Serialize;
use std::io::Write;

use super::dates;
use super::error::{CliError, CliErrorKind};

pub fn print_json<T: Serialize>(value: &T) -> Result<(), CliError> {
    serde_json::to_writer_pretty(std::io::stdout(), value)
        .map_err(|e| CliError::new(CliErrorKind::Other, format!("write JSON: {e}")))?;
    println!();
    std::io::stdout()
        .flush()
        .map_err(|e| CliError::new(CliErrorKind::Other, format!("flush JSON: {e}")))?;
    Ok(())
}

pub fn print_progress(label: &str, done: u64, total: u64, last_percent: &mut u64) {
    let percent = done.saturating_mul(100).checked_div(total).unwrap_or(100);
    if percent != *last_percent {
        eprint!("\r{}: {}% ({}/{})", label, percent, done, total);
        let _ = std::io::stderr().flush();
        *last_percent = percent;
    }
}

pub fn finish_progress() {
    eprintln!();
}

#[derive(Debug, Serialize)]
pub struct DeviceRow {
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial_number: Option<String>,
    pub location_id: u64,
    pub location: String,
    pub speed: Option<String>,
    pub match_reason: String,
}

impl From<&MtpDeviceInfo> for DeviceRow {
    fn from(info: &MtpDeviceInfo) -> Self {
        Self {
            vendor_id: info.vendor_id,
            product_id: info.product_id,
            manufacturer: info.manufacturer.clone(),
            product: info.product.clone(),
            serial_number: info.serial_number.clone(),
            location_id: info.location_id,
            location: format!("{:08x}", info.location_id),
            speed: info.speed.map(|speed| format!("{speed:?}")),
            match_reason: info.match_reason.as_str().to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StorageRow {
    pub index: usize,
    pub id: String,
    pub id_raw: u64,
    pub description: String,
    pub volume_identifier: String,
    pub max_capacity: u64,
    pub free_space_bytes: u64,
    pub access_capability: String,
    pub storage_type: String,
    pub filesystem_type: String,
}

impl StorageRow {
    pub fn from_storage(index: usize, storage: &Storage) -> Self {
        Self {
            index,
            id: format!("{:08x}", storage.id().0),
            id_raw: storage.id().0,
            description: storage.info().description.clone(),
            volume_identifier: storage.info().volume_identifier.clone(),
            max_capacity: storage.info().total_capacity,
            free_space_bytes: storage.info().free_space,
            access_capability: if storage.info().is_writable {
                "ReadWrite".to_string()
            } else {
                "ReadOnly".to_string()
            },
            storage_type: format!("{:?}", storage.info().storage_type),
            filesystem_type: format!("{:?}", storage.info().filesystem_type),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ObjectRow {
    pub handle: u64,
    pub storage_id: u64,
    pub parent: u64,
    pub filename: String,
    pub kind: String,
    pub size: u64,
    pub format: String,
    /// ISO 8601, with an offset only when the device sent one (see [`dates::iso`]). `null` when
    /// the device sent no date or one that doesn't parse.
    pub modified: Option<String>,
    /// Like `modified`. Often `null`: Android's fast folder listing carries no creation date.
    pub created: Option<String>,
}

impl From<&ObjectInfo> for ObjectRow {
    fn from(info: &ObjectInfo) -> Self {
        Self {
            handle: info.handle.0,
            storage_id: info.storage_id.0,
            parent: info.parent.0,
            filename: info.filename.clone(),
            kind: if info.is_folder() { "folder" } else { "file" }.to_string(),
            size: info.size,
            format: format!("{:#06x}", info.format.code()),
            modified: info.modified.as_ref().map(dates::iso),
            created: info.created.as_ref().map(dates::iso),
        }
    }
}
