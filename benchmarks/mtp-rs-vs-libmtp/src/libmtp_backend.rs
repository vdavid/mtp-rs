// libmtp-rs backend for benchmarking MTP file transfers via the C libmtp library.
//
// System dependency: requires the `libmtp` C library to be installed on the system.
//   - macOS:  `brew install libmtp`
//   - Debian: `apt install libmtp-dev`
//   - Fedora: `dnf install libmtp-devel`

use libmtp_rs::device::raw::detect_raw_devices;
use libmtp_rs::device::{MtpDevice, StorageSort};
use libmtp_rs::object::filetypes::Filetype;
use libmtp_rs::object::Object;
use libmtp_rs::storage::files::FileMetadata;
use libmtp_rs::storage::Parent;
use libmtp_rs::util::HandlerReturn;
use std::cell::Cell;

/// Backend that wraps `libmtp-rs` (C libmtp) for MTP device operations.
pub struct LibmtpBackend {
    device: MtpDevice,
    /// ID of the temporary benchmark folder on the device.
    bench_folder_id: u32,
}

impl LibmtpBackend {
    /// Connect to the MTP device with this serial (or the only one, when `serial` is `None`) and
    /// create a temporary benchmark folder ("mtp-bench-tmp") under the root of the default
    /// storage.
    ///
    /// libmtp's raw device list has no serial without opening each device, and opening another
    /// phone could disturb whoever uses it. So the serial is resolved to a USB vendor and product
    /// ID through mtp-rs's listing (which opens nothing), and exactly one raw device must match
    /// them. After opening, the serial libmtp reads (the MTP DeviceInfo one, which can differ from
    /// the USB serial) is checked against `mtp_serial`, which mtp-rs read from the same device.
    pub fn connect(
        serial: Option<&str>,
        mtp_serial: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let raw_devices =
            detect_raw_devices().map_err(|e| format!("Failed to detect raw devices: {:?}", e))?;

        let ids: Vec<(u16, u16)> = raw_devices
            .iter()
            .map(|r| {
                let e = r.device_entry();
                (e.vendor_id, e.product_id)
            })
            .collect();
        let wanted = match serial {
            Some(s) => {
                let listed = mtp_rs::mtp::MtpDevice::list_devices()?;
                let d = listed
                    .iter()
                    .find(|d| d.serial_number.as_deref() == Some(s))
                    .ok_or_else(|| format!("no MTP device with serial {s}"))?;
                Some((d.vendor_id, d.product_id))
            }
            None => None,
        };
        let index = pick_raw_device(&ids, wanted)?;
        let raw = &raw_devices[index];

        let mut device = raw
            .open_uncached()
            .ok_or("Failed to open MTP device (open_uncached returned None)")?;
        if let Some(s) = mtp_serial {
            let got = device.serial_number().unwrap_or_default();
            if got != s {
                return Err(format!("libmtp opened MTP serial '{got}', expected '{s}'").into());
            }
        }

        device
            .update_storage(StorageSort::NotSorted)
            .map_err(|e| format!("Failed to update storage: {:?}", e))?;

        let pool = device.storage_pool();
        let (folder_id, _name) = pool
            .create_folder("mtp-bench-tmp", Parent::Root)
            .map_err(|e| format!("Failed to create benchmark folder: {:?}", e))?;

        Ok(Self {
            device,
            bench_folder_id: folder_id,
        })
    }

    /// Upload `data` as `filename` into the benchmark folder. Returns a file ID (u32).
    pub fn upload(&self, filename: &str, data: &[u8]) -> Result<u32, Box<dyn std::error::Error>> {
        let metadata = FileMetadata {
            file_size: data.len() as u64,
            file_name: filename,
            file_type: Filetype::Unknown,
            modification_date: chrono::Utc::now(),
        };

        let offset = Cell::new(0usize);
        let pool = self.device.storage_pool();
        let file = pool
            .send_file_from_handler(
                |buf: &mut [u8]| {
                    let pos = offset.get();
                    if pos >= data.len() {
                        return HandlerReturn::Ok(0);
                    }
                    let remaining = &data[pos..];
                    let to_copy = remaining.len().min(buf.len());
                    buf[..to_copy].copy_from_slice(&remaining[..to_copy]);
                    offset.set(pos + to_copy);
                    HandlerReturn::Ok(to_copy as u32)
                },
                Parent::Folder(self.bench_folder_id),
                metadata,
            )
            .map_err(|e| format!("Failed to upload file '{}': {:?}", filename, e))?;

        Ok(file.id())
    }

    /// Download the file with the given ID, returning all bytes.
    pub fn download(&self, file_id: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut result = Vec::new();
        let pool = self.device.storage_pool();
        pool.get_file_to_handler(file_id, |chunk: &[u8]| {
            result.extend_from_slice(chunk);
            HandlerReturn::Ok(chunk.len() as u32)
        })
        .map_err(|e| format!("Failed to download file {}: {:?}", file_id, e))?;

        Ok(result)
    }

    /// List files in the root of the storage. Returns the count.
    pub fn list_objects(&self) -> Result<usize, Box<dyn std::error::Error>> {
        let pool = self.device.storage_pool();
        let files = pool.files_and_folders(Parent::Root);
        Ok(files.len())
    }

    /// Delete a single object by ID.
    pub fn delete(&self, object_id: u32) -> Result<(), Box<dyn std::error::Error>> {
        let dummy = self.device.dummy_object(object_id);
        dummy
            .delete()
            .map_err(|e| format!("Failed to delete object {}: {:?}", object_id, e))?;
        Ok(())
    }

    /// Clean up the benchmark folder (deletes children first, then the folder).
    pub fn cleanup(self) -> Result<(), Box<dyn std::error::Error>> {
        // Delete children first: some devices don't support recursive folder deletion.
        let pool = self.device.storage_pool();
        let children = pool.files_and_folders(Parent::Folder(self.bench_folder_id));
        for child in children {
            let dummy = self.device.dummy_object(child.id());
            let _ = dummy.delete(); // best-effort
        }

        let dummy = self.device.dummy_object(self.bench_folder_id);
        dummy
            .delete()
            .map_err(|e| format!("Failed to delete benchmark folder: {:?}", e))?;
        Ok(())
    }

    /// Human-readable device description.
    pub fn device_description(&self) -> String {
        let manufacturer = self
            .device
            .manufacturer_name()
            .unwrap_or_else(|_| "Unknown".to_string());
        let model = self
            .device
            .model_name()
            .unwrap_or_else(|_| "Unknown".to_string());
        format!("{} {} (libmtp)", manufacturer, model)
    }
}

/// Which raw device to open: the only one matching `wanted` (vendor ID, product ID), or with no
/// `wanted`, the only one there is. Never a guess between several.
fn pick_raw_device(
    ids: &[(u16, u16)],
    wanted: Option<(u16, u16)>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let matches: Vec<usize> = ids
        .iter()
        .enumerate()
        .filter(|(_, id)| wanted.is_none_or(|w| **id == w))
        .map(|(i, _)| i)
        .collect();
    match (matches.as_slice(), wanted) {
        ([i], _) => Ok(*i),
        ([], Some((v, p))) => Err(format!("libmtp sees no device {v:04x}:{p:04x}").into()),
        ([], None) => Err("No MTP devices found".into()),
        (_, Some((v, p))) => Err(format!(
            "libmtp sees {} devices {v:04x}:{p:04x} and can't tell them apart without opening them",
            matches.len()
        )
        .into()),
        (_, None) => Err(format!(
            "{} MTP devices connected; pick one with --serial",
            matches.len()
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::pick_raw_device;

    const PIXEL: (u16, u16) = (0x18d1, 0x4ee1);
    const S23: (u16, u16) = (0x04e8, 0x6860);

    #[test]
    fn picks_by_usb_id_and_never_guesses() {
        assert_eq!(pick_raw_device(&[S23, PIXEL], Some(PIXEL)).unwrap(), 1);
        assert_eq!(pick_raw_device(&[PIXEL], None).unwrap(), 0);
        assert!(pick_raw_device(&[S23, PIXEL], None).is_err());
        assert!(pick_raw_device(&[S23], Some(PIXEL)).is_err());
        assert!(pick_raw_device(&[PIXEL, PIXEL], Some(PIXEL)).is_err());
        assert!(pick_raw_device(&[], None).is_err());
    }
}
