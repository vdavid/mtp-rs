# Architecture

This document describes the internal architecture and design decisions for `mtp-rs`.

## Layer diagram

```
┌─────────────────────────────────────────────────────────────┐
│                      User Application                        │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                     Public API Layer                         │
│  ┌─────────────────────┐    ┌─────────────────────────┐     │
│  │   mtp::MtpDevice    │    │    ptp::PtpDevice       │     │
│  │   mtp::Storage      │    │    ptp::PtpSession      │     │
│  │   mtp::ObjectInfo   │    │                         │     │
│  │   (media-focused)   │    │    (camera-focused)     │     │
│  └──────────┬──────────┘    └────────────┬────────────┘     │
│             │                            │                   │
│             └────────────┬───────────────┘                   │
└──────────────────────────┼──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                    Protocol Layer                            │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                   ptp::Session                        │   │
│  │  - Transaction management                             │   │
│  │  - Operation execution                                │   │
│  │  - Response handling                                  │   │
│  │  - Event listening                                    │   │
│  └──────────────────────────┬───────────────────────────┘   │
│                             │                                │
│  ┌──────────────────────────▼───────────────────────────┐   │
│  │                   ptp::Container                      │   │
│  │  - Command/Data/Response/Event containers             │   │
│  │  - Serialization/deserialization                      │   │
│  └──────────────────────────┬───────────────────────────┘   │
│                             │                                │
│  ┌──────────────────────────▼───────────────────────────┐   │
│  │                   ptp::pack                           │   │
│  │  - Primitive type encoding (u16, u32, u64)            │   │
│  │  - String encoding (UTF-16LE)                         │   │
│  │  - Array encoding                                     │   │
│  │  - Dataset structures (DeviceInfo, ObjectInfo, etc.)  │   │
│  └──────────────────────────────────────────────────────┘   │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                    Transport Layer                           │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                 transport::Transport                  │   │
│  │  (trait)                                              │   │
│  │  - send_bulk(&[u8])                                   │   │
│  │  - send_bulk_streaming(Stream<Bytes>)                 │   │
│  │  - receive_bulk() -> Vec<u8>                          │   │
│  │  - receive_interrupt() -> Event                       │   │
│  │  - cancel_transfer()                                  │   │
│  └──────────────────────────┬───────────────────────────┘   │
│                             │                                │
│  ┌──────────────────────────▼───────────────────────────┐   │
│  │              transport::NusbTransport                 │   │
│  │  - nusb device wrapper                                │   │
│  │  - Endpoint management                                │   │
│  │  - Async USB operations                               │   │
│  └──────────────────────────────────────────────────────┘   │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                       nusb crate                             │
│                   (USB device access)                        │
└─────────────────────────────────────────────────────────────┘
```

> The diagram shows the PTP-over-USB stack. As of the backend-neutral refactor, `mtp::` no longer
> sits directly on `ptp::`: it dispatches through an `MtpBackend` seam (`Box<dyn MtpBackend>`) with two
> implementations — `UsbBackend` (the stack above) and `WpdBackend` (Windows WPD-over-COM,
> `cfg(windows)`, which bypasses `ptp::`/`transport::` entirely). `ptp::PtpDevice`/`PtpSession` remain a
> separate low-level, USB-only public API for cameras. See "Backend seam" below.

## Module organization

```
src/
├── lib.rs                 # Crate root, re-exports
│
├── error.rs               # Error types
│
├── ptp/                   # PTP protocol implementation
│   ├── mod.rs             # Module exports
│   ├── codes.rs           # Operation, response, event codes
│   ├── container.rs       # USB container format
│   ├── pack.rs            # Binary serialization
│   ├── types.rs           # DeviceInfo, ObjectInfo, StorageInfo
│   ├── session.rs         # PTP session management
│   └── device.rs          # PtpDevice (public low-level API)
│
├── mtp/                   # High-level, backend-neutral API
│   ├── mod.rs             # Module exports
│   ├── device.rs          # MtpDevice, MtpDeviceBuilder, backend selection
│   ├── storage.rs         # Storage (façade over the active backend)
│   ├── types.rs           # Neutral ObjectHandle/StorageId/ObjectInfo/DeviceInfo/Capabilities/…
│   ├── error.rs           # Neutral mtp::Error + UploadError
│   ├── object.rs          # NewObjectInfo, ObjectFormat
│   ├── event.rs           # DeviceEvent
│   ├── hotplug.rs         # watch_devices, DeviceWatch, HotplugEvent
│   ├── stream.rs          # FileDownload, WindowedDownload, Progress
│   └── backend/           # The MtpBackend seam
│       ├── mod.rs         # MtpBackend trait, Backend selector, ByteRange
│       ├── usb.rs         # UsbBackend (PTP over Transport)
│       └── wpd/           # WpdBackend (Windows WPD-over-COM, cfg(windows))
│
└── transport/             # USB transport abstraction
    ├── mod.rs             # Transport trait, exports
    ├── nusb.rs            # nusb implementation
    ├── virtual_device/    # Filesystem-backed virtual device (feature = "virtual-device")
    └── mock.rs            # Mock for testing
```

## Dependency rules

```
┌─────────────┐
│    mtp      │ ──────────┐
└─────────────┘           │
       │                  │
       ▼                  ▼
┌─────────────┐    ┌─────────────┐
│    ptp      │◄───│   error     │
└─────────────┘    └─────────────┘
       │                  ▲
       ▼                  │
┌─────────────┐           │
│  transport  │───────────┘
└─────────────┘
```

- **mtp** (incl. `mtp::backend`) depends on: `ptp`, `transport`, `error`
- **mtp::backend::usb** drives `ptp` over `transport`; **mtp::backend::wpd** drives Windows WPD COM
  directly (`cfg(windows)`, no `ptp`/`transport`)
- **ptp** depends on: `transport`, `error`
- **transport** depends on: `error`, external `nusb`
- **error** depends on: nothing internal

**Never**:
- `ptp` imports from `mtp`
- `transport` imports from `ptp` or `mtp`
- Circular dependencies

## Key design decisions

### 1. Two-level API

**Decision**: Provide both high-level `mtp::` and low-level `ptp::` APIs.

**Rationale**:
- `mtp::` provides a clean, safe API for media device operations
- `ptp::` allows camera support and advanced use cases
- Users can drop down to `ptp::` when needed without reimplementing everything

### 2. Transport trait abstraction

**Decision**: Abstract USB communication behind a `Transport` trait.

```rust
pub trait Transport: Send + Sync {
    async fn send_bulk(&self, data: &[u8]) -> Result<(), Error>;
    async fn send_bulk_streaming(&self, chunks: BulkStream) -> Result<(), Error>;
    async fn receive_bulk(&self, max_size: usize) -> Result<Vec<u8>, Error>;
    async fn receive_interrupt(&self) -> Result<Vec<u8>, Error>;
    async fn cancel_transfer(&self, transaction_id: u32, idle_timeout: Duration) -> Result<(), Error>;
}
```

**Rationale**:
- Enables unit testing with mock transport
- `send_bulk_streaming` allows memory-efficient uploads (has a default impl that buffers and calls `send_bulk`)
- `cancel_transfer` enables safe mid-stream download cancellation
- Clean separation of concerns

### 3. Storage as first-class object

**Decision**: Operations are methods on `Storage`, not `Device`.

```rust
// Yes
let files = storage.list_objects(None).await?;

// No
let files = device.list_objects(storage_id, None).await?;
```

**Rationale**:
- Prevents accidentally mixing up storage IDs
- More intuitive API
- Storage holds reference to device, enforces lifetime

### 4. Stream-based downloads

**Decision**: Downloads return `Stream<Item = Result<Chunk, Error>>`.

**Rationale**:
- Memory efficient for large files
- Natural progress tracking
- User controls buffering strategy
- Composable with async ecosystem

### 5. No runtime dependency

**Decision**: Library uses only `futures` traits, no tokio/async-std dependency.

**Rationale**:
- Works with any async runtime
- `nusb` is already runtime-agnostic
- Smaller dependency footprint

### 6. Newtype wrappers for IDs

**Decision**: `ObjectHandle(u32)`, `StorageId(u32)` instead of raw `u32`.

**Rationale**:
- Prevents mixing up object handles and storage IDs
- Self-documenting code
- Zero runtime cost

## Concurrency model

### Session serialization

MTP transactions are synchronous at the protocol level - only one operation can be in progress at a time. The `PtpSession` ensures this:

```rust
struct PtpSession {
    transport: Arc<NusbTransport>,
    transaction_id: AtomicU32,
    lock: AsyncMutex<()>,  // Ensures one operation at a time
}

impl PtpSession {
    async fn execute(&self, op: OperationCode, params: &[u32]) -> Result<Response> {
        let _guard = self.lock.lock().await;  // Serialize operations
        // ... execute operation
    }
}
```

### Thread safety

- `MtpDevice` is `Send + Sync`
- Multiple `Storage` references can exist (they hold `Arc<Device>`)
- Concurrent operations are serialized internally
- Event stream can be polled from any task

### Cancellation and cleanup

**Download cancellation**: Call `FileDownload::cancel(idle_timeout)` (or `ReceiveStream::cancel()`) to safely abort a mid-stream download. This sends a USB Still Image Class cancel control request (bRequest=0x64) to the device, then drains remaining data from the USB pipes. The session remains healthy for subsequent operations. Dropping without calling `cancel()` corrupts the session (`debug_assert` catches this in debug builds). The implementation follows libmtp's proven approach; see `NusbTransport::cancel_transfer()` for details.

**Upload cancellation**: If an upload future is dropped after `SendObjectInfo` succeeds but before `SendObject` completes, a partial/empty object may remain on the device. The protocol has no abort mechanism. Callers should track the handle and delete incomplete objects if needed.

**Session cleanup**: `MtpDevice::close()` sends `CloseSession` best-effort, ignoring errors. Dropping an `MtpDevice` sends nothing: `Drop` can't run async work in a runtime-agnostic crate, the same reason in-session recovery is lazy. A device-side session can therefore outlive the host process, and the next `PtpSession::open` meets `SessionAlreadyOpen`; it closes and reopens so the transaction counter starts clean.

## Error handling

### Error propagation

Errors bubble up through layers with context:

```
Transport error (nusb::Error)
         │
         ▼
    Error::Usb(...)
         │
         ▼
Protocol error (bad response code)
         │
         ▼
    Error::Protocol { code, operation }
```

### Retryable errors

Some errors are transient:

```rust
impl Error {
    pub fn is_retryable(&self) -> bool {
        matches!(self,
            Error::Protocol { code: ResponseCode::DeviceBusy, .. } |
            Error::Timeout
        )
    }
}
```

## File ownership

| File                | Responsibility                                |
|---------------------|-----------------------------------------------|
| `lib.rs`            | Crate root, public re-exports                 |
| `error.rs`          | All error types                               |
| `ptp/codes.rs`      | Operation, response, event, format code enums |
| `ptp/pack.rs`       | Binary serialization primitives               |
| `ptp/container.rs`  | USB container format                          |
| `ptp/types.rs`      | DeviceInfo, StorageInfo, ObjectInfo structs   |
| `ptp/session.rs`    | Transaction management, operation execution   |
| `ptp/device.rs`     | PtpDevice public API                          |
| `mtp/device.rs`     | MtpDevice, MtpDeviceBuilder                   |
| `mtp/storage.rs`    | Storage struct and methods                    |
| `mtp/object.rs`     | ObjectInfo, NewObjectInfo, ObjectFormat       |
| `mtp/event.rs`      | DeviceEvent enum, event stream                |
| `mtp/hotplug.rs`    | Device arrival/departure watch                |
| `mtp/stream.rs`     | FileDownload, upload helpers                |
| `transport/mod.rs`  | Transport trait                               |
| `transport/nusb.rs` | nusb implementation                           |
| `transport/mock.rs` | Mock for testing                              |
