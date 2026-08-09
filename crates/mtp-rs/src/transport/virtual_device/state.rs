//! Internal state for the virtual MTP device.

use super::builders::build_event;
use super::config::{VirtualDeviceConfig, VirtualStorageConfig};
use crate::ptp::{EventCode, ObjectHandle, StorageId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// Per-storage state.
#[derive(Debug)]
pub(super) struct VirtualStorageState {
    pub config: VirtualStorageConfig,
    pub storage_id: StorageId,
}

/// A tracked virtual object (file or directory).
#[derive(Debug, Clone)]
pub(super) struct VirtualObject {
    /// Path relative to the storage backing dir.
    pub rel_path: PathBuf,
    /// Which storage this object belongs to.
    pub storage_id: StorageId,
    /// Parent handle (`ObjectHandle::ROOT` for root-level objects).
    pub parent: ObjectHandle,
}

/// Pending SendObjectInfo state (MTP requires SendObjectInfo before SendObject).
#[derive(Debug, Clone)]
pub(super) struct PendingSendInfo {
    pub storage_id: StorageId,
    pub parent: ObjectHandle,
    pub filename: String,
    #[allow(dead_code)] // Part of the protocol state, kept for debugging
    pub size: u64,
    pub is_folder: bool,
    pub assigned_handle: ObjectHandle,
}

/// A command waiting for its data phase from the host.
///
/// When the host sends a command that expects a data phase (SendObjectInfo,
/// SendObject, SetObjectPropValue), the command is stored here. The next
/// `send_bulk` (data container) takes it and dispatches both together.
#[derive(Debug)]
pub(super) struct PendingCommand {
    pub code: u16,
    pub tx_id: u32,
    pub params: Vec<u32>,
}

/// Full mutable state of the virtual device.
#[derive(Debug)]
pub(super) struct VirtualDeviceState {
    pub config: VirtualDeviceConfig,
    pub session_open: bool,
    pub next_handle: u32,
    pub objects: HashMap<u32, VirtualObject>,
    pub storages: Vec<VirtualStorageState>,
    pub pending_send: Option<PendingSendInfo>,
    pub pending_command: Option<PendingCommand>,
    pub event_queue: VecDeque<Vec<u8>>,
    pub response_queue: VecDeque<Vec<u8>>,
    /// Refcount of in-flight pause guards. When > 0, the filesystem watcher
    /// silently drops all events (and records them in `dropped_paths` so test
    /// code can observe a sentinel arriving). Refcounted so multiple concurrent
    /// test drains compose correctly: the watcher only resumes when the last
    /// guard drops. Managed by [`WatcherGuard`](super::registry::WatcherGuard)
    /// via [`pause_watcher`](super::registry::pause_watcher).
    pub(super) pause_count: u32,
    /// Canonical paths the watcher dropped while paused. Bounded (oldest evicted
    /// past `DROPPED_PATHS_CAP`). Test code writes a unique sentinel file after
    /// recreating fixtures and polls this set to confirm the watcher's queue
    /// has drained past that point: per-directory ordering on every supported
    /// `notify` backend means the sentinel's event arrives after every event
    /// from any preceding write to the same directory.
    pub(super) dropped_paths: VecDeque<PathBuf>,
    /// Test hook: byte caps to apply to the next `GetPartialObject(64)` reads,
    /// one popped per read (front first). A cap clamps how many bytes that read
    /// returns below what was requested: `0` yields an **empty** data container
    /// (a device stalling mid-file), `n > 0` a **short** read of `n` real bytes
    /// (a legal partial read). Once the queue is empty, reads behave normally.
    /// Set via [`force_partial_read_caps`](super::registry::force_partial_read_caps).
    /// Exercises the windowed-download stall path (0 bytes while bytes remain ⇒
    /// error, not EOF) and short-read bookkeeping (advance by bytes returned).
    pub(super) forced_partial_read_caps: VecDeque<usize>,

    /// One-shot: when set, the next `cancel_transfer` returns
    /// [`PtpError::DeviceReset`](crate::PtpError::DeviceReset), modeling the
    /// Samsung cancel wedge that the real USB transport detects and recovers
    /// from via a device reset (issue #18). Lets the high-level `DeviceReset`
    /// contract (cancel → error → consumer reopens) be tested with no hardware.
    /// Set via [`force_cancel_wedge`](super::registry::force_cancel_wedge).
    pub(super) pending_cancel_wedge: bool,

    /// One-shot: when set, the next PTP **operation** (command container) fails
    /// with [`PtpError::DeviceReset`](crate::PtpError::DeviceReset). The sibling
    /// of `pending_cancel_wedge`, for consumers that never call `cancel()`: they
    /// reach `DeviceReset` through
    /// [`recover_if_needed`](crate::ptp::PtpSession)'s drain after a dropped
    /// operation future, which no cancel-armed hook can reproduce. Set via
    /// [`force_operation_wedge`](super::registry::force_operation_wedge).
    pub(super) pending_operation_wedge: bool,

    /// Test hook: object handles whose `GetObjectInfo` answers with a response
    /// code instead of an ObjectInfo dataset, mapped to that code. The object
    /// stays present and readable by every other operation, which is the point:
    /// this models a device that enumerates a handle and then won't describe it
    /// (Sphaira on the Nintendo Switch does this for one handle out of 50,
    /// issue #22). Sticky rather than one-shot, so a re-listing behaves the same
    /// way twice. Set via
    /// [`force_object_info_error`](super::registry::force_object_info_error).
    pub(super) forced_object_info_errors: HashMap<u32, u16>,
}

/// Maximum number of entries in [`VirtualDeviceState::dropped_paths`]; oldest
/// is evicted on push past this. 1024 comfortably holds the events from a few
/// hundred concurrent fixture-recreate drains; bounds memory at ~160 KB worst
/// case (1024 × ~150 byte `PathBuf` average), so long-running test sessions
/// can't leak.
///
/// If your test scenario pauses the watcher for multi-minute spans and
/// generates more than 1024 FS events you care about, file an issue: a
/// runtime-configurable cap is the natural next step, but every consumer so
/// far has been comfortably under the default.
pub const DROPPED_PATHS_CAP: usize = 1024;

impl VirtualDeviceState {
    /// Create initial state from config.
    pub fn new(config: VirtualDeviceConfig) -> Self {
        let storages: Vec<VirtualStorageState> = config
            .storages
            .iter()
            .enumerate()
            .map(|(i, sc)| {
                // Canonicalize backing dirs so that all paths (from handlers, watcher,
                // dedup tracker) use the same form. On macOS, /var → /private/var.
                let mut resolved_config = sc.clone();
                if let Ok(canonical) = sc.backing_dir.canonicalize() {
                    resolved_config.backing_dir = canonical;
                }
                VirtualStorageState {
                    config: resolved_config,
                    // Storage IDs conventionally start at 0x00010001
                    storage_id: StorageId(0x0001_0001 + i as u32),
                }
            })
            .collect();

        Self {
            config,
            session_open: false,
            next_handle: 1,
            objects: HashMap::new(),
            storages,
            pending_send: None,
            pending_command: None,
            event_queue: VecDeque::new(),
            response_queue: VecDeque::new(),
            pause_count: 0,
            dropped_paths: VecDeque::new(),
            forced_partial_read_caps: VecDeque::new(),
            pending_cancel_wedge: false,
            pending_operation_wedge: false,
            forced_object_info_errors: HashMap::new(),
        }
    }

    /// Allocate the next object handle.
    pub fn alloc_handle(&mut self) -> ObjectHandle {
        let h = self.next_handle;
        self.next_handle += 1;
        ObjectHandle(h)
    }

    /// Reassign the object handle of the object at `rel_path`, keeping the object
    /// (and its on-disk contents) in place. The OLD handle becomes invalid (the
    /// device answers `InvalidObjectHandle` / `InvalidParentObject` for it), while
    /// a fresh listing of the parent returns the NEW handle. Direct children are
    /// re-parented to the new handle so `GetObjectInfo` keeps reporting a live
    /// parent.
    ///
    /// This simulates Android MediaProvider re-keying object IDs across a media
    /// rescan: the folder still exists, but a handle the host cached before the
    /// rescan is now stale. Unlike
    /// [`rescan_backing_dirs`](Self::rescan_backing_dirs) it queues no events:
    /// it models the device moving on silently, before the host has observed
    /// anything, which is exactly the window where a cached parent handle goes
    /// stale under a host's upload.
    ///
    /// Returns `(old_handle, new_handle)`, or `None` if no tracked object matches
    /// `rel_path`. Matching is by `rel_path` across all storages, so pass a path
    /// unique enough to disambiguate if the same relative path exists on two
    /// storages. The object must have been listed at least once (so it's tracked)
    /// before it can be re-keyed.
    pub fn rekey_object(&mut self, rel_path: &Path) -> Option<(ObjectHandle, ObjectHandle)> {
        let old_key = self
            .objects
            .iter()
            .find(|(_, obj)| obj.rel_path.as_path() == rel_path)
            .map(|(handle, _)| *handle)?;
        let new_handle = self.alloc_handle();
        let obj = self.objects.remove(&old_key)?;
        self.objects.insert(new_handle.0, obj);

        let old_handle = ObjectHandle(old_key);
        for child in self.objects.values_mut() {
            if child.parent == old_handle {
                child.parent = new_handle;
            }
        }
        Some((old_handle, new_handle))
    }

    /// Find a storage state by ID.
    pub fn find_storage(&self, id: StorageId) -> Option<&VirtualStorageState> {
        self.storages.iter().find(|s| s.storage_id == id)
    }

    /// Find or create handles for all entries in a directory.
    /// Returns handles for direct children of `parent` within the given storage.
    pub fn scan_dir(
        &mut self,
        storage_id: StorageId,
        parent: ObjectHandle,
    ) -> Result<Vec<ObjectHandle>, std::io::Error> {
        let storage = self
            .storages
            .iter()
            .find(|s| s.storage_id == storage_id)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "storage not found")
            })?;

        let base_dir = &storage.config.backing_dir;

        // Determine the filesystem path for this parent
        let dir_path = if parent == ObjectHandle::ROOT || parent.0 == 0 {
            base_dir.clone()
        } else {
            match self.objects.get(&parent.0) {
                Some(obj) if obj.storage_id == storage_id => base_dir.join(&obj.rel_path),
                _ => return Ok(Vec::new()),
            }
        };

        if !dir_path.is_dir() {
            return Ok(Vec::new());
        }

        let mut handles = Vec::new();
        let entries = std::fs::read_dir(&dir_path)?;

        for entry in entries {
            let entry = entry?;
            let file_name = entry.file_name();
            let rel = if parent == ObjectHandle::ROOT || parent.0 == 0 {
                PathBuf::from(&file_name)
            } else {
                let parent_obj = self.objects.get(&parent.0).unwrap();
                parent_obj.rel_path.join(&file_name)
            };

            // Check if we already have a handle for this path in this storage
            let existing = self
                .objects
                .iter()
                .find(|(_, obj)| obj.storage_id == storage_id && obj.rel_path == rel);

            let handle = if let Some((&h, _)) = existing {
                ObjectHandle(h)
            } else {
                let h = self.alloc_handle();
                self.objects.insert(
                    h.0,
                    VirtualObject {
                        rel_path: rel,
                        storage_id,
                        parent,
                    },
                );
                h
            };

            handles.push(handle);
        }

        Ok(handles)
    }

    /// Recursively scan all objects in a storage.
    pub fn scan_all(&mut self, storage_id: StorageId) -> Result<Vec<ObjectHandle>, std::io::Error> {
        // First scan root
        let root_handles = self.scan_dir(storage_id, ObjectHandle::ROOT)?;
        let mut all_handles = root_handles.clone();

        // BFS through directories
        let mut queue = root_handles;
        while let Some(handle) = queue.pop() {
            let obj = match self.objects.get(&handle.0) {
                Some(o) => o.clone(),
                None => continue,
            };
            let storage = match self.find_storage(storage_id) {
                Some(s) => s,
                None => continue,
            };
            let full_path = storage.config.backing_dir.join(&obj.rel_path);
            if full_path.is_dir() {
                let children = self.scan_dir(storage_id, handle)?;
                all_handles.extend(&children);
                queue.extend(children);
            }
        }

        Ok(all_handles)
    }

    /// Resolve the full filesystem path for an object handle.
    pub fn resolve_path(&self, handle: ObjectHandle) -> Option<PathBuf> {
        let obj = self.objects.get(&handle.0)?;
        let storage = self.find_storage(obj.storage_id)?;
        Some(storage.config.backing_dir.join(&obj.rel_path))
    }

    /// Check if a storage is read-only.
    pub fn is_read_only(&self, storage_id: StorageId) -> bool {
        self.find_storage(storage_id)
            .map(|s| s.config.read_only)
            .unwrap_or(false)
    }

    /// Force a full rescan of all backing directories, syncing the in-memory
    /// object tree with the actual filesystem state.
    ///
    /// This diffs the current `objects` map against the filesystem:
    /// - **Stale entries** (objects whose `rel_path` no longer exists on disk) are removed.
    /// - **New entries** (files on disk not tracked in `objects`) are added.
    ///
    /// Appropriate `ObjectAdded`, `ObjectRemoved`, and `StorageInfoChanged`
    /// events are queued for each change so MTP clients can pick them up
    /// via `receive_interrupt()`.
    ///
    /// Use this when you've manipulated the backing directories directly
    /// (for example, resetting test fixtures) and need the virtual device to
    /// reflect the changes immediately, without waiting for the filesystem
    /// watcher's latency.
    pub fn rescan_backing_dirs(&mut self) -> RescanSummary {
        let mut added = 0u32;
        let mut removed = 0u32;
        let mut changed_storages = HashSet::new();

        // Phase 1: Remove stale entries (objects whose files no longer exist on disk).
        let stale_handles: Vec<(u32, StorageId)> = self
            .objects
            .iter()
            .filter_map(|(&handle, obj)| {
                let storage = self
                    .storages
                    .iter()
                    .find(|s| s.storage_id == obj.storage_id)?;
                let full_path = storage.config.backing_dir.join(&obj.rel_path);
                if !full_path.exists() {
                    Some((handle, obj.storage_id))
                } else {
                    None
                }
            })
            .collect();

        for (handle, storage_id) in stale_handles {
            self.objects.remove(&handle);
            self.event_queue
                .push_back(build_event(EventCode::ObjectRemoved, &[handle]));
            changed_storages.insert(storage_id);
            removed += 1;
        }

        // Phase 2: Add new entries by scanning all storages recursively.
        // Collect storage info upfront to avoid borrow conflicts.
        let storage_info: Vec<(StorageId, PathBuf)> = self
            .storages
            .iter()
            .map(|s| (s.storage_id, s.config.backing_dir.clone()))
            .collect();

        for (storage_id, backing_dir) in &storage_info {
            let storage_added =
                self.rescan_dir(*storage_id, backing_dir, backing_dir, ObjectHandle::ROOT);
            added += storage_added;
            if storage_added > 0 {
                changed_storages.insert(*storage_id);
            }
        }

        // Phase 3: Queue StorageInfoChanged for each affected storage.
        for storage_id in &changed_storages {
            self.event_queue
                .push_back(build_event(EventCode::StorageInfoChanged, &[storage_id.0]));
        }

        RescanSummary { added, removed }
    }

    /// Recursively scan a directory and add any entries not already tracked.
    /// Returns the number of newly added objects.
    fn rescan_dir(
        &mut self,
        storage_id: StorageId,
        backing_dir: &std::path::Path,
        dir_path: &std::path::Path,
        parent: ObjectHandle,
    ) -> u32 {
        let entries = match std::fs::read_dir(dir_path) {
            Ok(e) => e,
            Err(_) => return 0,
        };

        let mut added = 0u32;

        for entry in entries.flatten() {
            let full_path = entry.path();
            let rel_path = match full_path.strip_prefix(backing_dir) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };

            // Check if already tracked.
            let existing = self
                .objects
                .iter()
                .find(|(_, obj)| obj.storage_id == storage_id && obj.rel_path == rel_path)
                .map(|(&h, _)| ObjectHandle(h));

            let handle = if let Some(h) = existing {
                h
            } else {
                let h = self.alloc_handle();
                self.objects.insert(
                    h.0,
                    VirtualObject {
                        rel_path: rel_path.clone(),
                        storage_id,
                        parent,
                    },
                );
                self.event_queue
                    .push_back(build_event(EventCode::ObjectAdded, &[h.0]));
                added += 1;
                h
            };

            // Recurse into directories.
            if full_path.is_dir() {
                added += self.rescan_dir(storage_id, backing_dir, &full_path, handle);
            }
        }

        added
    }
}

/// Summary of changes made by a rescan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RescanSummary {
    /// Number of new objects added to the object tree.
    pub added: u32,
    /// Number of stale objects removed from the object tree.
    pub removed: u32,
}
