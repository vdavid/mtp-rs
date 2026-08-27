# Virtual device transport

Feature-gated (`virtual-device`) Transport implementation backed by local filesystem directories instead of USB. Exercises the full MTP/PTP binary protocol path.

## Architecture

```
MtpDevice (unchanged)
  → MtpDeviceInner (unchanged)
    → PtpSession (unchanged)
      → Arc<dyn Transport>
        → VirtualTransport: implements Transport trait
          → VirtualDeviceState: in-memory object tree + filesystem
```

## Module structure

- `config.rs`: `VirtualDeviceConfig`, `VirtualStorageConfig` (public types). `supports_partial_object` and `supports_partial_object_64` shape the read capability: each one gates both the `OperationsSupported` entry and the dispatch arm, so an un-advertised op answers `Operation_Not_Supported` rather than being quietly served. Both `false` makes `Capabilities::supports_partial_download` report `false` and leaves `GetObject` as the only way to read an object, which is libhaze (Sphaira on the Switch) and most simple PTP responders
- `state.rs`: `VirtualDeviceState`, `VirtualObject`, `PendingCommand`, handle management
- `builders.rs`: binary payload builders (DeviceInfo, StorageInfo, ObjectInfo, containers)
- `handlers.rs`: protocol operation handlers dispatched by opcode
- `registry.rs`: global virtual device registry for discovery integration (`list_devices`, `open_by_location`, `open_by_serial`) + active-state registry for `rescan_virtual_device()`, `pause_watcher()`/`WatcherGuard`, and the fault-injection hooks `force_partial_read_caps()` (short/stall reads), `force_cancel_wedge()` (one-shot: next `cancel_transfer` returns `Error::DeviceReset`), and `force_operation_wedge()` (one-shot: next operation returns it, for consumers that never call `cancel()`). Both wedge hooks model the #18 Samsung cancel wedge with no hardware; neither models its aftermath (a real session stays dead until a spaced-retry reopen). Plus `force_object_info_error()` / `clear_object_info_errors()` (sticky, by handle: `GetObjectInfo` answers a response code while the object stays present and readable, modeling the Sphaira partially-readable folder in #22). Its config twin is `VirtualDeviceConfig::undescribable_objects`, keyed by storage-relative path, for a device that has to come up already broken because the consumer under test runs in another process
- `watcher.rs`: filesystem watcher for detecting out-of-band changes to backing directories
- `mod.rs`: `VirtualTransport` struct + `Transport` impl + tests

## Key decisions

- **`std::sync::Mutex`** over `parking_lot::Mutex`: `parking_lot` is only a dev-dep. Virtual transport isn't performance-critical, so std mutex is fine.
- **`PendingCommand` struct**: When the host sends a command that expects a data phase (SendObjectInfo, SendObject, SetObjectPropValue), the command is stored as a `PendingCommand` in `state.pending_command`. The next `send_bulk` (data container) takes it via `.take()` and dispatches both together. This keeps pending state separate from the response queue.
- **`VecDeque` for queues**: `response_queue` and `event_queue` use `VecDeque` for O(1) front removal (FIFO access pattern).
- **Discovery via global registry**: Virtual devices can be registered via `register_virtual_device()` to appear in `MtpDevice::list_devices()`. They get synthetic location IDs starting at `0xFFFF_0000_0000_0000` to avoid collisions with real USB devices. Uses `OnceLock` for the static registry.
- **Event poll interval**: `VirtualTransport` stores `event_poll_interval: Duration` outside the mutex. When no events are pending, `receive_interrupt` awaits this delay before returning `Timeout`, preventing CPU spin in event loops. Tests use `Duration::ZERO` for speed; production callers should use 50ms+.
- **Filesystem watcher**: Controlled by `VirtualDeviceConfig::watch_backing_dirs`. When `true`, a `notify::RecommendedWatcher` watches all backing dirs recursively. When files are written/deleted directly (bypassing MTP), the watcher detects changes and queues `ObjectAdded`/`ObjectRemoved` events. Gated behind `virtual-device` feature via the `notify` dependency. Tests that don't need the watcher should set this to `false` for faster startup and no background threads.
- **Watcher scope**: The filesystem watcher only tracks file/directory creation and removal. Content modifications to existing files are intentionally ignored: they don't change the object tree and would be noisy (editors write temp files, do atomic renames, etc.). Real MTP devices are also inconsistent about emitting `ObjectInfoChanged` for content edits.
- **Dedup for watcher events**: Uses state-based dedup rather than TTL tracking. MTP handlers modify the filesystem while holding the `state` mutex and insert/remove handles before releasing the lock. The watcher callback also acquires `state` before processing events. For creates, the watcher skips events when a handle already exists for the path. For removes, the watcher skips when no handle is found (already removed by the MTP handler). No extra tracking structure or timing assumptions needed. Events for the backing directory itself (empty relative path) are skipped: macOS FSEvents reports the watched directory as "created" on startup.
- **Canonical backing dirs**: `VirtualDeviceState::new()` canonicalizes all backing dirs at startup. This ensures consistent path comparison between handlers and the watcher callback (important on macOS where `/var` → `/private/var`).
- **Rescan via active-state registry**: `VirtualTransport::new()` registers its `Arc<Mutex<VirtualDeviceState>>` in a second global registry keyed by serial number. `rescan_virtual_device(serial)` looks up the state and calls `rescan_backing_dirs()`, which diffs the in-memory object tree against the filesystem, removing stale entries and adding new ones. The transport unregisters on drop. This avoids the fs watcher's latency (200-500ms on macOS FSEvents) and handles rapid delete+recreate sequences that the watcher can miss.
- **Watcher pause/resume (refcounted)**: `pause_watcher(serial)` returns a `WatcherGuard` (RAII) that increments `pause_count` on the device state. While `pause_count > 0`, the watcher callback drops all events AND records the canonical path in `dropped_paths` (a `VecDeque` capped at `DROPPED_PATHS_CAP = 1024`, oldest evicted past that). The guard decrements `pause_count` on drop (poison-safe via `lock().ok()`); the watcher actually resumes only when the count returns to zero, so concurrent drains compose. This prevents the race where external code deletes and recreates files in the backing directory: without pausing, the OS can deliver stale deletion events after a rescan has already re-added the objects. The `dropped_paths` ring is the observation surface for tests that want event-driven drain confirmation instead of fixed sleeps (`dropped_paths_since_pause` / `was_path_dropped` / `clear_dropped_paths`); see AGENTS.md § "Test-time backing-dir drain" for the sentinel-file pattern.

## Gotchas

- `list_objects(None)` applies a parent filter (`ParentFilter::Exact(ROOT)`), so the virtual transport must set `parent = ObjectHandle::ROOT` on root-level objects for them to appear.
- `SendObjectInfo` creates the object on disk immediately (folder via `create_dir_all`; file as an empty placeholder), matching real devices where `SendObjectInfo` yields a real, addressable handle before the data phase. Folders need no `SendObject`, so `pending_send` clears for them; files keep `pending_send` set, and `SendObject` overwrites the placeholder with the real bytes. This is what makes `UploadError::partial` truthful: a mid-stream upload failure leaves a real, queryable, deletable empty object at the surfaced handle (resume or delete, the consumer's call). Event-wise, `SendObjectInfo` emits `ObjectAdded` (once, at creation) and `SendObject` emits `ObjectInfoChanged` (data changed), so the watcher-dedup contract of exactly one `ObjectAdded` per upload holds.
- Storage IDs start at `0x00010001` (matching real MTP convention).
- The global registries (device registry + active-state registry) are process-wide and shared across tests. Registry tests must clean up with `unregister_virtual_device()` and use unique serial numbers to avoid interference. Rescan tests must also use unique serials.
- `event_poll_interval` lives on `VirtualTransport` (not inside `VirtualDeviceState`) because we need it after dropping the mutex lock and before an async `.await`.
- Fs watcher tests use canonicalized backing dirs and `poll_event_with_retry` to handle macOS FSEvents latency. The removal test drains create events first because macOS may coalesce or reorder events.
- `GetPartialObject64` (0x95C1) has a **different param layout** from `GetPartialObject` (0x101B): 4 params (handle, offset_lo, offset_hi, max_bytes) vs 3 (handle, offset, max_bytes). The two handlers share `read_partial()` for the actual file I/O but parse params separately.
