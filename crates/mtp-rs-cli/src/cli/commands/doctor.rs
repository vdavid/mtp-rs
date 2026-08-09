use mtp_rs::{ByteRange, MtpDevice, ObjectHandle, Storage};
use serde::Serialize;
use std::collections::VecDeque;
use std::time::Duration;

use crate::cli::args::{Cli, DoctorArgs};
use crate::cli::device::open_selected_device;
use crate::cli::error::{CliError, CliErrorKind};
use crate::cli::helpers::existing_object;
use crate::cli::output::{print_json, DeviceRow, StorageRow};
use crate::cli::path::RemotePath;

#[derive(Debug, Serialize)]
struct DoctorRow {
    devices: Vec<DeviceRow>,
    opened: Option<OpenedDeviceRow>,
    open_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_help: Option<String>,
    storages: Vec<DoctorStorageRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cancel_probe: Option<CancelProbeRow>,
}

#[derive(Debug, Serialize)]
struct OpenedDeviceRow {
    manufacturer: String,
    model: String,
    serial_number: String,
    capabilities: CapabilitiesRow,
}

#[derive(Debug, Serialize)]
struct CapabilitiesRow {
    can_upload: bool,
    can_delete: bool,
    can_rename: bool,
    can_move: bool,
    can_copy: bool,
    can_create_folder: bool,
    supports_partial_download: bool,
    supports_thumbnails: bool,
    supports_events: bool,
}

/// Outcome of the `--probe-cancel` cancel-health check (the #18 reproducer):
/// download a file, cancel mid-stream, and see whether the session survives.
#[derive(Debug, Serialize)]
struct CancelProbeRow {
    /// What the probe did, in one word: `healthy`, `wedged_recovered`,
    /// `errored`, or `skipped`.
    outcome: &'static str,
    /// Human-readable detail (file used, error text, or why it was skipped).
    detail: String,
}

#[derive(Debug, Serialize)]
struct DoctorStorageRow {
    storage: StorageRow,
    root_listed: bool,
    writable_folder_hints: Vec<String>,
    /// Handles the device enumerated at the root but wouldn't describe. Exactly
    /// the kind of half-working device doctor exists to name, and invisible in a
    /// plain listing.
    unreadable_root_objects: Vec<UnreadableObjectRow>,
}

#[derive(Debug, Serialize)]
struct UnreadableObjectRow {
    handle: u64,
    error: String,
}

pub async fn run(cli: &Cli, args: &DoctorArgs) -> Result<(), CliError> {
    let devices = MtpDevice::list_devices_with_known(&cli.known)
        .map_err(|e| CliError::from_mtp("list devices", e, cli.verbose))?;
    if devices.is_empty() {
        if cli.json {
            print_json(&DoctorRow {
                devices: Vec::new(),
                opened: None,
                open_error: None,
                open_help: None,
                storages: Vec::new(),
                cancel_probe: None,
            })?;
        } else {
            println!("devices: none");
        }
        return Err(CliError::new(CliErrorKind::NoDevice, "no MTP device found"));
    }
    let device_rows: Vec<DeviceRow> = devices.iter().map(DeviceRow::from).collect();
    if !cli.json {
        println!("devices: {} visible", devices.len());
        for device in &devices {
            println!("  {}", device.display());
        }
    }

    let device = match open_selected_device(cli).await {
        Ok(device) => device,
        Err(err) => {
            if cli.json {
                print_json(&DoctorRow {
                    devices: device_rows,
                    opened: None,
                    open_error: Some(err.to_string()),
                    open_help: err.help().map(str::to_string),
                    storages: Vec::new(),
                    cancel_probe: None,
                })?;
            }
            return Err(err);
        }
    };
    let caps = device.capabilities();
    let opened = OpenedDeviceRow {
        manufacturer: device.device_info().manufacturer.clone(),
        model: device.device_info().model.clone(),
        serial_number: device.device_info().serial_number.clone(),
        capabilities: CapabilitiesRow {
            can_upload: caps.can_upload,
            can_delete: caps.can_delete,
            can_rename: caps.can_rename,
            can_move: caps.can_move,
            can_copy: caps.can_copy,
            can_create_folder: caps.can_create_folder,
            supports_partial_download: caps.supports_partial_download,
            supports_thumbnails: caps.supports_thumbnails,
            supports_events: caps.supports_events,
        },
    };
    if !cli.json {
        println!("open: ok ({} {})", opened.manufacturer, opened.model);
        let c = &opened.capabilities;
        println!(
            "capabilities: upload={} delete={} rename={} move={} copy={} mkdir={} partial_download={} thumbnails={} events={}",
            c.can_upload,
            c.can_delete,
            c.can_rename,
            c.can_move,
            c.can_copy,
            c.can_create_folder,
            c.supports_partial_download,
            c.supports_thumbnails,
            c.supports_events,
        );
    }

    let storages = device
        .storages()
        .await
        .map_err(|e| CliError::from_mtp("list storages", e, cli.verbose))?;
    let mut storage_rows = Vec::new();
    if !cli.json {
        println!("storages: {}", storages.len());
    }
    for (index, storage) in storages.iter().enumerate() {
        if !cli.json {
            println!(
                "  [{}] {} free={} access={}",
                index,
                storage.info().description,
                storage.info().free_space,
                if storage.info().is_writable {
                    "ReadWrite"
                } else {
                    "ReadOnly"
                }
            );
        }
        let root_collection = storage
            .collect_objects(None)
            .await
            .map_err(|e| CliError::from_mtp("list storage root", e, cli.verbose))?;
        let root = root_collection.objects;
        let unreadable: Vec<UnreadableObjectRow> = root_collection
            .skipped
            .iter()
            .map(|s| UnreadableObjectRow {
                handle: s.handle.0,
                error: s.error.to_string(),
            })
            .collect();
        if !cli.json && !unreadable.is_empty() {
            println!(
                "      unreadable at root: {} object(s) the device listed but won't describe",
                unreadable.len()
            );
            for entry in &unreadable {
                println!("        handle={} {}", entry.handle, entry.error);
            }
        }
        let hints: Vec<String> = [
            "Download",
            "Downloads",
            "Documents",
            "Music",
            "Pictures",
            "Audiobooks",
            "Podcasts",
            "GARMIN",
        ]
        .into_iter()
        .filter(|name| {
            root.iter()
                .any(|object| object.is_folder() && object.filename == *name)
        })
        .map(str::to_string)
        .collect();
        if !cli.json {
            if hints.is_empty() {
                println!("      writable-folder hints: none found at root");
            } else {
                println!("      writable-folder hints: {}", hints.join(", "));
            }
        }
        storage_rows.push(DoctorStorageRow {
            storage: StorageRow::from_storage(index, storage),
            root_listed: true,
            writable_folder_hints: hints,
            unreadable_root_objects: unreadable,
        });
    }

    let cancel_probe = if args.probe_cancel || args.probe_path.is_some() {
        let row = match storages.first() {
            Some(storage) => {
                cancel_health_probe(storage, args.probe_path.as_deref(), cli.verbose).await
            }
            None => CancelProbeRow {
                outcome: "skipped",
                detail: "no storage to probe".to_string(),
            },
        };
        if !cli.json {
            println!("cancel-probe: {} ({})", row.outcome, row.detail);
        }
        Some(row)
    } else {
        None
    };

    if cli.json {
        return print_json(&DoctorRow {
            devices: device_rows,
            opened: Some(opened),
            open_error: None,
            open_help: None,
            storages: storage_rows,
            cancel_probe,
        });
    }

    Ok(())
}

/// A file the cancel probe can download and cancel.
#[derive(Debug, Clone)]
struct ProbeTarget {
    handle: ObjectHandle,
    /// Absolute remote path, for the human-readable detail line.
    path: String,
    size: u64,
}

/// The size band the search prefers. Anything in it is good enough to stop at.
const PROBE_PREFERRED_MIN_SIZE: u64 = 100_000;
const PROBE_PREFERRED_MAX_SIZE: u64 = 10_000_000;
/// How far below the storage root the search descends, and how many folders it
/// lists in total. The probe is a diagnostic, not a crawler: it must not walk a
/// whole phone before reporting.
const PROBE_MAX_DEPTH: usize = 3;
const PROBE_MAX_FOLDERS: usize = 48;

/// Picks which file the probe downloads.
///
/// **Size barely matters for wedge detection**: a Galaxy S23 Ultra wedged on a
/// 36-byte file (verified on SM-S918B, macOS/nusb, 2026-07-20). So a mid-size
/// file is a preference, never a requirement: any file beats skipping the probe.
#[derive(Debug, Default)]
struct ProbePick {
    best: Option<ProbeTarget>,
}

impl ProbePick {
    /// Offer a candidate. Returns `true` when the pick is final, so the search
    /// can stop walking.
    fn offer(&mut self, candidate: ProbeTarget) -> bool {
        let preferred =
            (PROBE_PREFERRED_MIN_SIZE..=PROBE_PREFERRED_MAX_SIZE).contains(&candidate.size);
        if preferred {
            self.best = Some(candidate);
            return true;
        }
        if self
            .best
            .as_ref()
            .is_none_or(|best| candidate.size > best.size)
        {
            self.best = Some(candidate);
        }
        false
    }
}

/// Visit order for a folder's subfolders: user files first, `Android` last.
///
/// `Android/data` and `Android/obb` are huge and partly unreadable over MTP, so
/// walking them first burns the folder budget for nothing.
fn folder_priority(name: &str) -> u8 {
    match name {
        "DCIM" | "Camera" | "Download" | "Downloads" | "Pictures" => 0,
        "Movies" | "Music" | "Documents" | "Audiobooks" | "Podcasts" => 1,
        "Android" => 3,
        _ => 2,
    }
}

/// Find a file to probe with, searching below the root.
///
/// An Android MTP root is the top of shared storage and by convention holds
/// only directories (`DCIM`, `Download`, `Android`, …), so a clean phone has
/// zero files there. Looking at the root alone skipped the probe on exactly the
/// devices issue #18 is about (verified on a Pixel 9 Pro XL: 17 directories,
/// zero files). Hence the breadth-first walk, bounded by [`PROBE_MAX_DEPTH`] and
/// [`PROBE_MAX_FOLDERS`].
///
/// The `Err` string explains what the search covered, so a skip is actionable.
async fn find_probe_target(storage: &Storage) -> Result<ProbeTarget, String> {
    let mut queue: VecDeque<(Option<ObjectHandle>, String, usize)> =
        VecDeque::from([(None, String::new(), 0)]);
    let mut pick = ProbePick::default();
    let mut folders_listed = 0usize;

    while let Some((parent, prefix, depth)) = queue.pop_front() {
        if folders_listed >= PROBE_MAX_FOLDERS {
            break;
        }
        folders_listed += 1;
        let objects = match storage.list_objects(parent).await {
            Ok(objects) => objects,
            // An unreadable subfolder is normal on Android; only a root that
            // won't list is worth reporting.
            Err(e) if parent.is_none() => return Err(format!("could not list storage root: {e}")),
            Err(_) => continue,
        };

        let mut subfolders = Vec::new();
        for object in objects {
            let path = format!("{prefix}/{}", object.filename);
            if object.is_file() {
                let candidate = ProbeTarget {
                    handle: object.handle,
                    path,
                    size: object.size,
                };
                if pick.offer(candidate) {
                    return Ok(pick.best.expect("a final pick is always set"));
                }
            } else if object.is_folder() && depth < PROBE_MAX_DEPTH {
                subfolders.push((object.handle, path, object.filename));
            }
        }
        subfolders.sort_by_key(|(_, _, name)| folder_priority(name));
        for (handle, path, _) in subfolders {
            queue.push_back((Some(handle), path, depth + 1));
        }
    }

    pick.best.ok_or_else(|| {
        format!(
            "found no file to probe in {folders_listed} folder(s), searching up to \
             {PROBE_MAX_DEPTH} levels below the storage root. Pass --probe-path /some/file \
             to point the probe at one"
        )
    })
}

/// Resolve an explicit `--probe-path`. The `Err` string is the skip detail.
async fn resolve_probe_path(
    storage: &Storage,
    path: &str,
    verbose: bool,
) -> Result<ProbeTarget, String> {
    let remote_path = RemotePath::parse(path).map_err(|e| e.to_string())?;
    let object = existing_object(storage, &remote_path, verbose)
        .await
        .map_err(|e| e.to_string())?;
    if !object.is_file() {
        return Err(format!("--probe-path {path} is not a file"));
    }
    Ok(ProbeTarget {
        handle: object.handle,
        path: path.to_string(),
        size: object.size,
    })
}

/// The cancel-health probe (`--probe-cancel`): download a file, cancel
/// mid-stream, and classify what happened. This is the #18 reproducer — a device
/// that wedges on a cancel returns `DeviceReset` here, which the plain listing
/// above can't reveal. Read-only.
async fn cancel_health_probe(
    storage: &Storage,
    probe_path: Option<&str>,
    verbose: bool,
) -> CancelProbeRow {
    let target = match probe_path {
        Some(path) => resolve_probe_path(storage, path, verbose).await,
        None => find_probe_target(storage).await,
    };
    let target = match target {
        Ok(target) => target,
        Err(detail) => {
            return CancelProbeRow {
                outcome: "skipped",
                detail,
            };
        }
    };

    let mut download = match storage.download(target.handle, ByteRange::Full).await {
        Ok(download) => download,
        Err(e) => {
            return CancelProbeRow {
                outcome: "errored",
                detail: format!("could not start download of '{}': {e}", target.path),
            };
        }
    };
    // Read one chunk so there is an in-flight transfer to cancel.
    let _ = download.next_chunk().await;

    match download.cancel(Duration::from_millis(300)).await {
        Ok(()) => CancelProbeRow {
            outcome: "healthy",
            detail: format!(
                "cancelled '{}' ({} bytes); session survived",
                target.path, target.size
            ),
        },
        Err(mtp_rs::Error::DeviceReset) => CancelProbeRow {
            outcome: "wedged_recovered",
            detail: format!(
                "cancel wedged the device on '{}' ({} bytes); the library reset it to recover (#18). \
                 Reopen quietly to continue, and prefer download_windowed for interruptible reads",
                target.path, target.size
            ),
        },
        Err(e) => CancelProbeRow {
            outcome: "errored",
            detail: format!("cancel returned an error: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(size: u64) -> ProbeTarget {
        ProbeTarget {
            handle: ObjectHandle(1),
            path: format!("/f{size}"),
            size,
        }
    }

    #[test]
    fn a_mid_size_file_ends_the_search() {
        let mut pick = ProbePick::default();
        assert!(pick.offer(target(200_000)));
        assert_eq!(pick.best.unwrap().size, 200_000);
    }

    #[test]
    fn a_tiny_file_is_kept_rather_than_skipped() {
        // 36 bytes wedged a Galaxy S23 Ultra, so small files are worth probing.
        let mut pick = ProbePick::default();
        assert!(!pick.offer(target(36)));
        assert_eq!(pick.best.unwrap().size, 36);
    }

    #[test]
    fn outside_the_band_the_largest_file_wins() {
        let mut pick = ProbePick::default();
        assert!(!pick.offer(target(36)));
        assert!(!pick.offer(target(4_000)));
        assert!(!pick.offer(target(500)));
        assert_eq!(pick.best.unwrap().size, 4_000);
    }

    #[test]
    fn a_mid_size_file_beats_a_bigger_out_of_band_one() {
        let mut pick = ProbePick::default();
        assert!(!pick.offer(target(900_000_000)));
        assert!(pick.offer(target(150_000)));
        assert_eq!(pick.best.unwrap().size, 150_000);
    }

    #[test]
    fn user_folders_are_visited_before_the_android_tree() {
        let mut names = vec!["Android", "Notifications", "DCIM", "Music"];
        names.sort_by_key(|name| folder_priority(name));
        assert_eq!(names, vec!["DCIM", "Music", "Notifications", "Android"]);
    }
}
