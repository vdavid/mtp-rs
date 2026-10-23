//! The per-device COM worker thread and its message protocol.
//!
//! WPD COM pointers are apartment-affine and `!Send`, so one dedicated [`std::thread`] owns the
//! [`WpdDevice`] and does *all* COM work; everything else talks to it over channels. The thread
//! `CoInitializeEx`es an MTA, opens the device, then serves one [`Request`] at a time. [`WpdHandle`]
//! (held by the backend) carries only channel ends, so it is `Send + Sync` with no `unsafe`.

use super::com::{self, WpdDevice};
use super::props::map_hresult;
use crate::cancel::CancelToken;
use crate::mtp::backend::ByteRange;
use crate::mtp::object::NewObjectInfo;
use crate::mtp::{
    Capabilities, DeviceEvent, DeviceInfo, Error, ObjectHandle, ObjectInfo, StorageId, StorageInfo,
};
use bytes::Bytes;
use futures::channel::mpsc::UnboundedSender;
use futures::channel::{mpsc, oneshot};
use futures::executor::block_on;
use futures::{SinkExt, StreamExt};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, IStream, COINIT_MULTITHREADED};

/// Bytes read from the device per `IStream::Read`.
const CHUNK: usize = 256 * 1024;
/// How long the worker keeps a read stream open for the next window of a windowed download (see
/// [`OpenRead`]). Back-to-back windows arrive within milliseconds; a longer gap is a pause, and
/// the next window then reopens the stream and pays the prefix once.
const IDLE_READ_STREAM_RELEASE: std::time::Duration = std::time::Duration::from_secs(1);
/// How many chunks the download/upload channels buffer before back-pressuring the producer.
pub(crate) const DATA_BOUND: usize = 4;

/// Which device the worker should open.
pub(crate) enum OpenSpec {
    /// The first enumerated device.
    First,
    /// The device whose serial number matches.
    Serial(String),
    /// The physical USB device behind an nusb `location_id`. Matched by VID/PID (carried in the WPD
    /// PnP id); when more than one device shares that VID/PID (e.g. two identical phones) it's
    /// disambiguated by the USB descriptor `serial`, resolved from each candidate's parent in the
    /// device tree — the nusb and WPD *device* serials differ, so VID/PID + tree-serial is the
    /// reliable cross-reference.
    UsbDevice {
        /// The USB-descriptor serial nusb reported for the device at the location (if any).
        serial: Option<String>,
        /// USB vendor id.
        vid: u16,
        /// USB product id.
        pid: u16,
    },
}

/// The streaming-download reply: full object size plus the channel the chunks arrive on.
pub(crate) struct DownloadStart {
    pub(crate) size: u64,
    pub(crate) data: mpsc::Receiver<Result<Bytes, Error>>,
}

/// The streaming-upload verdict the worker sends back once the data channel closes.
///
/// Decouples the *device* outcome (committed / aborted / errored) from the *source* outcome (clean
/// end / cancel / source error) the consumer tracks, so the two are reconciled in [`WpdBackend::upload`].
pub(crate) enum UploadReply {
    /// All declared bytes arrived and the object was committed; carries its handle.
    Committed(ObjectHandle),
    /// The channel closed before `info.size` bytes (a cancel or a source error): the stream was
    /// released *without* `Commit`. `partial` is the handle of any object the device left behind
    /// (probed by re-listing the parent), or `None` if the abort left nothing.
    ShortClosed { partial: Option<ObjectHandle> },
    /// The device failed while creating the object or writing a chunk.
    Error(Error),
}

/// A unit of work for the COM worker. Each carries the oneshot it replies on.
pub(crate) enum Request {
    Storages(oneshot::Sender<Result<Vec<StorageInfo>, Error>>),
    StorageInfo(StorageId, oneshot::Sender<Result<StorageInfo, Error>>),
    List {
        storage: StorageId,
        parent: Option<ObjectHandle>,
        cancel: Option<CancelToken>,
        reply: oneshot::Sender<Result<Vec<ObjectInfo>, Error>>,
    },
    ObjectInfo(ObjectHandle, oneshot::Sender<Result<ObjectInfo, Error>>),
    Download {
        obj: ObjectHandle,
        range: ByteRange,
        reply: oneshot::Sender<Result<DownloadStart, Error>>,
    },
    ReadRange {
        obj: ObjectHandle,
        offset: u64,
        len: Option<u32>,
        reply: oneshot::Sender<Result<Vec<u8>, Error>>,
    },
    Thumbnail {
        obj: ObjectHandle,
        reply: oneshot::Sender<Result<Vec<u8>, Error>>,
    },
    CreateFolder {
        storage: StorageId,
        parent: Option<ObjectHandle>,
        name: String,
        reply: oneshot::Sender<Result<ObjectHandle, Error>>,
    },
    Upload {
        storage: StorageId,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        /// Bounded chunk channel the consumer forwards source bytes on (back-pressured to
        /// `DATA_BOUND` chunks). Closing it (dropping the sender) ends the upload: a clean close at
        /// `info.size` bytes commits, a short close aborts. Nothing buffers the whole file.
        data: mpsc::Receiver<Bytes>,
        reply: oneshot::Sender<UploadReply>,
    },
    Delete {
        obj: ObjectHandle,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    Rename {
        obj: ObjectHandle,
        name: String,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    MoveObject {
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    CopyObject {
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
        reply: oneshot::Sender<Result<ObjectHandle, Error>>,
    },
    Shutdown,
}

/// The backend's handle to a running worker. `Send + Sync`, holds only channel ends.
pub(crate) struct WpdHandle {
    req_tx: std_mpsc::Sender<Request>,
    join: Option<JoinHandle<()>>,
}

impl WpdHandle {
    /// Spawn a worker, open the device, and return the handle, the device's cached identity, and the
    /// receiving end of the device-event channel (the worker's WPD callback owns the sender).
    pub(crate) async fn spawn(
        spec: OpenSpec,
    ) -> Result<
        (
            Self,
            DeviceInfo,
            Capabilities,
            mpsc::UnboundedReceiver<DeviceEvent>,
        ),
        Error,
    > {
        let (req_tx, req_rx) = std_mpsc::channel::<Request>();
        let (startup_tx, startup_rx) =
            oneshot::channel::<Result<(DeviceInfo, Capabilities), Error>>();
        // The event channel is created here so the receiver can be returned to the backend; the
        // `Send` sender is moved into the worker, which hands it to the WPD callback on Advise.
        let (event_tx, event_rx) = mpsc::unbounded::<DeviceEvent>();

        let join = std::thread::Builder::new()
            .name("wpd-com-worker".into())
            .spawn(move || worker_main(spec, startup_tx, req_rx, event_tx))
            .map_err(|e| Error::Io {
                message: format!("failed to spawn WPD worker thread: {e}"),
            })?;

        let (device_info, capabilities) = startup_rx
            .await
            .map_err(|_| Error::Disconnected)? // worker died before replying
            ?; // the open result itself

        Ok((
            Self {
                req_tx,
                join: Some(join),
            },
            device_info,
            capabilities,
            event_rx,
        ))
    }

    /// Send a request and await its reply. Maps a dead worker to [`Error::Disconnected`].
    pub(crate) async fn call<T, F>(&self, make: F) -> Result<T, Error>
    where
        F: FnOnce(oneshot::Sender<Result<T, Error>>) -> Request,
    {
        let (tx, rx) = oneshot::channel();
        self.req_tx
            .send(make(tx))
            .map_err(|_| Error::Disconnected)?;
        rx.await.map_err(|_| Error::Disconnected)?
    }

    /// Fire-and-forget a request without awaiting its reply.
    ///
    /// Used by the streaming upload, where the consumer must drive the source channel *while* the
    /// worker processes the request, then await the reply separately. Maps a dead worker to
    /// [`Error::Disconnected`].
    pub(crate) fn send(&self, req: Request) -> Result<(), Error> {
        self.req_tx.send(req).map_err(|_| Error::Disconnected)
    }

    /// Best-effort shutdown signal (used by `close()`; `Drop` also sends one).
    pub(crate) fn shutdown(&self) {
        let _ = self.req_tx.send(Request::Shutdown);
    }
}

impl Drop for WpdHandle {
    fn drop(&mut self) {
        // Ask the worker to stop and wait for it to release COM + CoUninitialize on its own thread.
        let _ = self.req_tx.send(Request::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The worker thread entry point: COM init → open → serve requests → COM teardown.
fn worker_main(
    spec: OpenSpec,
    startup: oneshot::Sender<Result<(DeviceInfo, Capabilities), Error>>,
    req_rx: std_mpsc::Receiver<Request>,
    event_tx: UnboundedSender<DeviceEvent>,
) {
    // SAFETY: this is the only thread that touches these COM objects; MTA matches the spike.
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = startup.send(Err(map_hresult(e)));
            return;
        }
    }

    let mut dev = match unsafe { open_device(spec) } {
        Ok(dev) => {
            let _ = startup.send(Ok((dev.device_info().clone(), *dev.capabilities())));
            dev
        }
        Err(e) => {
            let _ = startup.send(Err(e));
            unsafe { CoUninitialize() };
            return;
        }
    };

    // Register the WPD event callback on *this* (the COM apartment) thread, now that the device is
    // chosen — `Advise` must run on the apartment that owns the device, and registering after
    // open_device avoids advising the candidate devices a `Serial` match opens and discards. The
    // sender is moved into the callback; if Advise fails the sink keeps it alive (the channel stays
    // open, so `next_event` simply blocks rather than reporting a phantom disconnect).
    unsafe { dev.register_events(event_tx) };

    let mut last_read: Option<OpenRead> = None;
    loop {
        // While a read stream is kept for the next window, wait only so long for that window. A
        // consumer that pauses between windows would otherwise keep the device busy for other
        // apps for as long as the pause lasts.
        let req = if last_read.is_some() {
            match req_rx.recv_timeout(IDLE_READ_STREAM_RELEASE) {
                Ok(req) => req,
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    last_read = None;
                    continue;
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match req_rx.recv() {
                Ok(req) => req,
                Err(_) => break,
            }
        };
        // An open read stream keeps the device busy (any other request fails with "device busy"),
        // so it is released before anything but the next read.
        if !matches!(req, Request::ReadRange { .. }) {
            last_read = None;
        }
        match req {
            Request::Shutdown => break,
            Request::Storages(reply) => {
                let _ = reply.send(unsafe { dev.storages() });
            }
            Request::StorageInfo(storage, reply) => {
                let _ = reply.send(unsafe { dev.storage_info(storage) });
            }
            Request::List {
                storage,
                parent,
                cancel,
                reply,
            } => {
                let _ = reply.send(unsafe { dev.list(storage, parent, cancel.as_ref()) });
            }
            Request::ObjectInfo(obj, reply) => {
                let _ = reply.send(unsafe { dev.object_info(obj) });
            }
            Request::Download { obj, range, reply } => handle_download(&mut dev, obj, range, reply),
            Request::ReadRange {
                obj,
                offset,
                len,
                reply,
            } => {
                let _ = reply.send(handle_read_range(
                    &mut dev,
                    obj,
                    offset,
                    len,
                    &mut last_read,
                ));
            }
            Request::Thumbnail { obj, reply } => {
                let _ = reply.send(handle_thumbnail(&mut dev, obj));
            }
            Request::CreateFolder {
                storage,
                parent,
                name,
                reply,
            } => {
                let _ = reply.send(unsafe { dev.create_folder(storage, parent, &name) });
            }
            Request::Upload {
                storage,
                parent,
                info,
                data,
                reply,
            } => handle_upload(&mut dev, storage, parent, info, data, reply),
            Request::Delete { obj, reply } => {
                let _ = reply.send(unsafe { dev.delete(obj) });
            }
            Request::Rename { obj, name, reply } => {
                let _ = reply.send(unsafe { dev.rename(obj, &name) });
            }
            Request::MoveObject {
                obj,
                new_parent,
                new_storage,
                reply,
            } => {
                let _ = reply.send(unsafe { dev.move_object(obj, new_parent, new_storage) });
            }
            Request::CopyObject {
                obj,
                new_parent,
                new_storage,
                reply,
            } => {
                let _ = reply.send(unsafe { dev.copy_object(obj, new_parent, new_storage) });
            }
        }
    }

    // Release every COM interface (the kept read stream, then the device) before uninitializing
    // COM on this thread. The loop also ends when the request channel closes without a `Shutdown`.
    drop(last_read);
    drop(dev);
    unsafe { CoUninitialize() };
}

/// Resolve an [`OpenSpec`] to an open [`WpdDevice`].
unsafe fn open_device(spec: OpenSpec) -> Result<WpdDevice, Error> {
    let entries = com::enumerate()?;
    match spec {
        OpenSpec::First => {
            let first = entries.first().ok_or(Error::NoDevice)?;
            WpdDevice::open(&first.pnp_id)
        }
        OpenSpec::Serial(serial) => {
            for entry in &entries {
                if let Ok(dev) = WpdDevice::open(&entry.pnp_id) {
                    if dev.device_info().serial_number == serial {
                        return Ok(dev);
                    }
                    // else: `dev` drops here, closing the non-matching device.
                }
            }
            Err(Error::NoDevice)
        }
        OpenSpec::UsbDevice { serial, vid, pid } => {
            // VID/PID lives in the PnP id, so candidates are found without opening anything.
            let candidates: Vec<&com::DeviceEntry> = entries
                .iter()
                .filter(|e| pnp_vid_pid(&e.pnp_id) == Some((vid, pid)))
                .collect();
            match candidates.as_slice() {
                [] => Err(Error::NoDevice),
                // Exactly one device with this VID/PID — unambiguous, no serial check needed.
                [only] => WpdDevice::open(&only.pnp_id),
                // Two+ identical-model devices share the VID/PID. Disambiguate by the USB serial
                // resolved from each candidate's parent in the device tree; refuse to guess if that
                // can't pin exactly one.
                many => {
                    if let Some(target) = serial.as_deref() {
                        for entry in many {
                            if com::wpd_device_usb_serial(&entry.pnp_id)
                                .is_some_and(|s| s.eq_ignore_ascii_case(target))
                            {
                                return WpdDevice::open(&entry.pnp_id);
                            }
                        }
                    }
                    Err(Error::Other {
                        detail: format!(
                            "{} WPD devices share VID/PID {vid:04x}:{pid:04x} and none matched the \
                             USB serial {serial:?}; open by (WPD) serial instead",
                            many.len()
                        ),
                    })
                }
            }
        }
    }
}

/// Extract `(vid, pid)` from a WPD PnP id like `\\?\usb#vid_18d1&pid_4ee2&mi_00#…`.
fn pnp_vid_pid(pnp_id: &str) -> Option<(u16, u16)> {
    let lower = pnp_id.to_ascii_lowercase();
    let hex4 = |marker: &str| -> Option<u16> {
        let digits = lower.split(marker).nth(1)?.get(0..4)?;
        u16::from_str_radix(digits, 16).ok()
    };
    Some((hex4("vid_")?, hex4("pid_")?))
}

/// Stream an object's bytes into a bounded channel, honoring the requested [`ByteRange`].
///
/// The full object size and the receiver are sent back *before* the read loop starts, so the
/// consumer can begin pulling immediately. Dropping/closing the receiver makes the next `send` fail,
/// which stops the loop and releases the `IStream` — that is the WPD cancel (no SIC needed).
fn handle_download(
    dev: &mut WpdDevice,
    obj: ObjectHandle,
    range: ByteRange,
    reply: oneshot::Sender<Result<DownloadStart, Error>>,
) {
    let size = match unsafe { dev.object_size(obj) } {
        Ok(s) => s,
        Err(e) => {
            let _ = reply.send(Err(e));
            return;
        }
    };
    let offset = range.offset();
    if offset > size {
        let _ = reply.send(Err(Error::invalid_data(format!(
            "download offset {offset} is past the object size {size}"
        ))));
        return;
    }

    let stream = match unsafe { dev.open_stream(obj) } {
        Ok(s) => s,
        Err(e) => {
            let _ = reply.send(Err(e));
            return;
        }
    };
    if let Err(e) = unsafe { com::stream_seek(&stream, offset) } {
        let _ = reply.send(Err(e));
        return;
    }

    let (mut tx, rx) = mpsc::channel::<Result<Bytes, Error>>(DATA_BOUND);
    if reply.send(Ok(DownloadStart { size, data: rx })).is_err() {
        return; // consumer already gave up
    }

    // Cap total bytes for a bounded `Range`; `Full`/`From` read to EOF.
    let mut remaining: Option<u64> = match range {
        ByteRange::Range { len, .. } => Some(len),
        _ => None,
    };
    let mut buf = vec![0u8; CHUNK];
    loop {
        let want = match remaining {
            Some(r) => (r as usize).min(buf.len()),
            None => buf.len(),
        };
        if want == 0 {
            break;
        }
        let n = match unsafe { com::stream_read(&stream, &mut buf[..want]) } {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) => {
                let _ = block_on(tx.send(Err(e)));
                break;
            }
        };
        if block_on(tx.send(Ok(Bytes::copy_from_slice(&buf[..n])))).is_err() {
            break; // consumer dropped → cancel
        }
        if let Some(r) = remaining.as_mut() {
            *r -= n as u64;
        }
    }
    // `tx` drops here → channel closes → consumer sees `None` (clean EOF).
}

/// The stream of the last [`handle_read_range`] that stopped before EOF, and where it stopped.
///
/// A windowed download asks for consecutive ranges of one object. Reopening the stream for each
/// window costs a `GetStream`, and on forward-only streams (`Seek` is `E_NOTIMPL`) re-reads the whole
/// prefix, so a large file took time quadratic in its size. A read starting exactly where the
/// previous one stopped continues on the same stream instead.
struct OpenRead {
    obj: ObjectHandle,
    at: u64,
    stream: IStream,
}

/// Buffered single-shot read of `[offset, offset+len)` (or to EOF when `len` is `None`).
fn handle_read_range(
    dev: &mut WpdDevice,
    obj: ObjectHandle,
    offset: u64,
    len: Option<u32>,
    last: &mut Option<OpenRead>,
) -> Result<Vec<u8>, Error> {
    // `take` releases a stream that does not continue here before another one is opened.
    let stream = match last
        .take()
        .filter(|open| open.obj == obj && open.at == offset)
    {
        Some(open) => open.stream,
        None => {
            let stream = unsafe { dev.open_stream(obj) }?;
            unsafe { com::stream_seek(&stream, offset) }?;
            stream
        }
    };

    let cap = len.map(|l| l as usize);
    let mut out = Vec::with_capacity(cap.unwrap_or(0));
    let mut buf = vec![0u8; CHUNK];
    let mut eof = false;
    loop {
        let want = match cap {
            Some(c) => (c - out.len()).min(buf.len()),
            None => buf.len(),
        };
        if want == 0 {
            break;
        }
        let n = unsafe { com::stream_read(&stream, &mut buf[..want]) }?;
        if n == 0 {
            eof = true;
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    if !eof {
        *last = Some(OpenRead {
            obj,
            at: offset + out.len() as u64,
            stream,
        });
    }
    Ok(out)
}

/// Read an object's thumbnail resource into a `Vec<u8>`.
///
/// Objects without a thumbnail resource fail at `GetStream`; the HRESULT maps to
/// `Unsupported`/`NotFound`, which the caller surfaces as-is.
fn handle_thumbnail(dev: &mut WpdDevice, obj: ObjectHandle) -> Result<Vec<u8>, Error> {
    let stream = unsafe { dev.open_thumbnail_stream(obj) }?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = unsafe { com::stream_read(&stream, &mut buf) }?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Stream an upload to the device chunk-by-chunk, never buffering the whole file.
///
/// Creates the object + data stream up front, then writes each chunk as it arrives over `rx`
/// (back-pressured by the bounded channel). When `rx` closes: a clean close at exactly `info.size`
/// bytes commits and returns the new handle; a short close releases the stream *without* committing
/// and probes the parent for any partial object the device left behind. A device-side create/write
/// failure short-circuits to [`UploadReply::Error`].
fn handle_upload(
    dev: &mut WpdDevice,
    storage: StorageId,
    parent: Option<ObjectHandle>,
    info: NewObjectInfo,
    mut rx: mpsc::Receiver<Bytes>,
    reply: oneshot::Sender<UploadReply>,
) {
    let stream = match unsafe { dev.create_upload_stream(storage, parent, &info) } {
        Ok(s) => s,
        Err(e) => {
            let _ = reply.send(UploadReply::Error(e));
            return;
        }
    };

    let mut written: u64 = 0;
    while let Some(chunk) = block_on(rx.next()) {
        if let Err(e) = unsafe { com::stream_write(&stream, &chunk) } {
            // Drop the stream (Release without Commit) before reporting the device error.
            drop(stream);
            let _ = reply.send(UploadReply::Error(e));
            return;
        }
        written += chunk.len() as u64;
    }

    if written == info.size {
        let result = unsafe { dev.commit_upload_stream(&stream) };
        let _ = reply.send(match result {
            Ok(handle) => UploadReply::Committed(handle),
            Err(e) => UploadReply::Error(e),
        });
    } else {
        // Short close: release WITHOUT Commit, then look for any partial the device kept.
        drop(stream);
        let partial = unsafe { dev.find_child_by_name(storage, parent, &info.filename) };
        let _ = reply.send(UploadReply::ShortClosed { partial });
    }
}
