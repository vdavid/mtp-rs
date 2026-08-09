//! The PTP-over-USB backend: today's [`PtpSession`] logic behind the neutral [`MtpBackend`] trait.
//!
//! All device-quirk handling lives here (the root-listing fast path and Android/Samsung/Fuji
//! fallbacks, the `>4 GB` size resolution, the upload partial-handle contract, SIC class-cancel,
//! and session self-healing). The trait boundary is the only place PTP types and the rich
//! [`crate::PtpError`] convert to the neutral [`crate::mtp`] vocabulary, via the `from_ptp`/`to_ptp`
//! helpers and the `From<PtpError>` error map. nusb, the virtual device, and the mock transport all
//! ride this one backend through their [`Transport`](crate::transport::Transport) impls.

use crate::cancel::{bail_if_cancelled, CancelToken};
use crate::mtp::backend::{
    BackendDownload, BackendListing, BackendListingError, ByteRange, DownloadBody, MtpBackend,
    ProgressFn, UploadStream,
};
use crate::mtp::object::NewObjectInfo;
use crate::mtp::stream::Progress;
use crate::mtp::{
    Capabilities, DeviceEvent, DeviceInfo, Error, ObjectHandle, ObjectInfo, StorageId, StorageInfo,
    UploadError,
};
use crate::ptp::{
    DeviceInfo as PtpDeviceInfo, ObjectHandle as PtpHandle, OperationCode, PtpSession,
    ReceiveStream, ResponseCode, StorageId as PtpStorageId,
};
use crate::PtpError;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

/// Which operation to use for a ranged/offset read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartialReadOp {
    /// `GetPartialObject64` (0x95C1): 64-bit offset, preferred when advertised.
    Wide,
    /// `GetPartialObject` (0x101B): 32-bit offset, the fallback for devices that
    /// don't advertise the 64-bit op (many PTP cameras, e.g. the Panasonic Lumix
    /// DMC-TZ61, issue #12). Only valid for offsets that fit in `u32`.
    Narrow,
}

/// Choose the partial-read op given what the device advertises and the target offset.
///
/// Prefers the 64-bit `GetPartialObject64`. Falls back to the 32-bit
/// `GetPartialObject` when the device lacks the 64-bit op but has the 32-bit one
/// and the offset fits in `u32` (so ranged/windowed/resumable downloads work on
/// cameras that only implement the 32-bit op, for any file up to 4 GiB). Errors
/// when the device advertises neither op, or when only the 32-bit op is available
/// but the offset is past 4 GiB (unreachable with a 32-bit offset).
fn plan_partial_read(
    has_partial_64: bool,
    has_partial_32: bool,
    offset: u64,
) -> Result<PartialReadOp, Error> {
    if has_partial_64 {
        Ok(PartialReadOp::Wide)
    } else if has_partial_32 {
        if offset > u64::from(u32::MAX) {
            Err(Error::invalid_data(format!(
                "offset {offset} needs GetPartialObject64 (64-bit), which this device doesn't \
                 advertise; its 32-bit GetPartialObject can't reach past 4 GiB"
            )))
        } else {
            Ok(PartialReadOp::Narrow)
        }
    } else {
        Err(Error::Unsupported)
    }
}

/// The PTP-over-USB implementation of [`MtpBackend`].
pub(crate) struct UsbBackend {
    session: Arc<PtpSession>,
    /// Neutral device identity, derived once at open.
    device_info: DeviceInfo,
    /// Neutral capabilities, derived once at open.
    capabilities: Capabilities,
    /// Which partial-object ops the device advertises, cached at open. Drives the
    /// ranged/windowed read path (see [`plan_partial_read`]).
    has_partial_object_64: bool,
    has_partial_object: bool,
}

impl UsbBackend {
    /// Build the backend from an open session, deriving neutral info/capabilities once.
    pub(crate) fn new(session: Arc<PtpSession>, ptp_info: PtpDeviceInfo) -> Self {
        let device_info = DeviceInfo::from_ptp(&ptp_info);
        let capabilities = Capabilities::from_ptp_device_info(&ptp_info);
        let has_partial_object_64 = ptp_info.supports_operation(OperationCode::GetPartialObject64);
        let has_partial_object = ptp_info.supports_operation(OperationCode::GetPartialObject);
        Self {
            session,
            device_info,
            capabilities,
            has_partial_object_64,
            has_partial_object,
        }
    }

    /// Decide how to issue a ranged/offset read for this device and offset.
    fn partial_read(&self, offset: u64) -> Result<PartialReadOp, Error> {
        plan_partial_read(self.has_partial_object_64, self.has_partial_object, offset)
    }

    /// Resolve the object-handle list for a listing, applying the root-listing quirks.
    ///
    /// Returns `(handles, filter)` in PTP terms. Mirrors the historical `list_objects_stream`
    /// fast-path/fallback logic exactly, just factored out so the streaming body can be built from
    /// it.
    async fn resolve_listing(
        &self,
        storage: PtpStorageId,
        parent: Option<PtpHandle>,
        cancel: Option<&CancelToken>,
    ) -> Result<(Vec<PtpHandle>, ParentFilter), PtpError> {
        // For root listings, try parent=0xFFFFFFFF first. Many devices (Android, Kindle, others)
        // return only root-level handles for this value, while parent=0 returns every object on the
        // storage. Fall back to parent=0 only when the device DECLINES 0xFFFFFFFF (see
        // `is_all_handle_rejection`); a session or transport failure propagates, because retrying
        // it would hammer a sick device and mask the real error.
        if parent.is_none() {
            match self
                .session
                .get_object_handles(storage, None, Some(PtpHandle::ALL))
                .await
            {
                Ok(handles) => {
                    let filter = root_filter(storage, &handles);
                    return Ok((handles, filter));
                }
                // Declined: fall through to the parent=0 path.
                Err(e) if is_all_handle_rejection(&e) => {}
                Err(e) => return Err(e),
            }
        }

        bail_if_cancelled(cancel)?;

        let result = self.session.get_object_handles(storage, None, parent).await;

        match result {
            Ok(handles) => {
                let filter = ParentFilter::Exact(parent.unwrap_or(PtpHandle::ROOT));
                Ok((handles, filter))
            }
            Err(PtpError::Protocol {
                code: ResponseCode::InvalidObjectHandle,
                ..
            }) if parent.is_none() => {
                // Samsung fallback: recursive listing filtered to root items. This is the
                // path where a storage-ID collision can actually bite, since the handle
                // set covers the whole storage (see `root_filter`).
                let handles = self
                    .session
                    .get_object_handles(storage, None, Some(PtpHandle::ALL))
                    .await?;
                let filter = root_filter(storage, &handles);
                Ok((handles, filter))
            }
            Err(e) => Err(e),
        }
    }
}

/// Did the device *decline* `GetObjectHandles(parent=0xFFFFFFFF)`, as opposed to
/// the session or the transport failing under it?
///
/// Only a decline may fall back to `parent=0`. Everything else has to propagate:
/// a second roundtrip on a sick device hammers it (a wedged Samsung re-wedges
/// into a hard `Timeout` under exactly that treatment, #18), and it would report
/// the *second* attempt's error, hiding the first. A consumer watching for
/// `Error::DeviceReset` to drive a quiet reopen would never see it, and a root
/// listing is the likeliest first call after a device goes sour.
///
/// Two shapes count as a decline:
///
/// - Selected `Protocol` responses that explicitly reject the operation or its
///   parent parameter (`OperationNotSupported`, `InvalidObjectHandle`,
///   `InvalidParentObject`, `InvalidParameter`, `ParameterNotSupported`). A
///   broad `GeneralError` is not a decline: enumeration integrity is unknown,
///   so it propagates.
/// - `Io`: how a bulk STALL arrives, which is how SIC-compliant cameras signal
///   an unsupported operation (Panasonic Lumix DMC-TZ61, #12). The transport
///   folds STALL in with `Fault`/`InvalidArgument`/`Unknown`, so `Io` can't be
///   narrowed further without a new `PtpError` variant; it stays on the
///   permissive side deliberately, since losing camera root listings would be
///   the worse failure.
fn is_all_handle_rejection(err: &PtpError) -> bool {
    matches!(
        err,
        PtpError::Protocol {
            code: ResponseCode::OperationNotSupported
                | ResponseCode::InvalidObjectHandle
                | ResponseCode::InvalidParentObject
                | ResponseCode::InvalidParameter
                | ResponseCode::ParameterNotSupported,
            ..
        } | PtpError::Io(_)
    )
}

/// Build the root filter for an enumerated handle set, deciding whether the
/// storage ID can be trusted as a root marker.
///
/// Some responders report the containing storage ID as the parent of every root
/// object (DBI and Sphaira/libhaze on the Nintendo Switch, PR #20). That reading
/// only holds while the storage ID isn't ALSO a real object handle: a storage ID
/// is a small number (`0x00010001` is 65,537), and on the recursive fallback path
/// the handle set covers the whole storage, so a device with a large library can
/// genuinely own a folder at that handle. Then `parent == storage_id` means
/// "inside that folder", and accepting it would list the folder's children
/// alongside the real root entries (duplicated subtrees for anything walking the
/// tree, since nested listings correctly use `Exact`).
///
/// So we look: if the storage ID came back as one of the objects, drop it as a
/// marker and fall back to the two reserved handles, which can never collide.
/// The check is one pass over the handle list, not per object.
fn root_filter(storage: PtpStorageId, handles: &[PtpHandle]) -> ParentFilter {
    let collides = handles.iter().any(|h| h.0 == storage.0);
    if collides {
        // Worth a line, because this is the one way the guard can bite back: on a
        // device that BOTH reports the storage ID as its root parent and owns a
        // folder at that handle, dropping the marker leaves nothing to accept and
        // the root lists as empty. That needs three coincidences and no known
        // device does it, but a silent empty root is miserable to diagnose from a
        // bug report, so say it out loud instead.
        diag_debug!(
            "root listing: storage ID {:#010x} is also an object handle here, so it can't double as \
             a root marker; accepting only the reserved parents (0, 0xFFFFFFFF). An unexpectedly \
             empty root listing on this device starts here.",
            storage.0
        );
    }
    ParentFilter::Root(if collides { None } else { Some(storage) })
}

/// How to filter objects by parent handle during a listing (PTP terms).
#[derive(Clone, Copy)]
enum ParentFilter {
    /// Accept objects whose parent matches exactly.
    Exact(PtpHandle),
    /// Device root: accept the reserved root handles (0 and 0xFFFFFFFF), plus the
    /// containing storage ID when [`root_filter`] established it can't be an
    /// object handle on this storage.
    Root(Option<PtpStorageId>),
}

impl ParentFilter {
    fn accepts(self, parent: PtpHandle) -> bool {
        match self {
            ParentFilter::Exact(expected) => parent == expected,
            ParentFilter::Root(storage) => {
                parent.0 == 0 || parent.0 == 0xFFFF_FFFF || storage.is_some_and(|s| parent.0 == s.0)
            }
        }
    }
}

/// State threaded through the listing `unfold` stream.
struct ListingState {
    session: Arc<PtpSession>,
    handles: Vec<PtpHandle>,
    cursor: usize,
    filter: ParentFilter,
    cancel: Option<CancelToken>,
}

/// The USB streaming-download body: a [`ReceiveStream`] with neutral error conversion.
struct UsbDownloadBody {
    stream: ReceiveStream,
}

#[async_trait]
impl DownloadBody for UsbDownloadBody {
    async fn next_chunk(&mut self) -> Option<Result<Bytes, Error>> {
        self.stream
            .next_chunk()
            .await
            .map(|r| r.map_err(Error::from))
    }

    async fn cancel(&mut self, idle_timeout: Duration) -> Result<(), Error> {
        self.stream.cancel(idle_timeout).await.map_err(Error::from)
    }
}

#[async_trait]
impl MtpBackend for UsbBackend {
    fn device_info(&self) -> &DeviceInfo {
        &self.device_info
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn storages(&self) -> Result<Vec<StorageInfo>, Error> {
        let ids = self.session.get_storage_ids().await?;
        let mut storages = Vec::with_capacity(ids.len());
        for id in ids {
            let info = self.session.get_storage_info(id).await?;
            let mut neutral = StorageInfo::from_ptp(&info);
            neutral.id = id.into();
            storages.push(neutral);
        }
        Ok(storages)
    }

    async fn storage_info(&self, storage: StorageId) -> Result<StorageInfo, Error> {
        let info = self.session.get_storage_info(storage.to_ptp()).await?;
        let mut neutral = StorageInfo::from_ptp(&info);
        neutral.id = storage;
        Ok(neutral)
    }

    async fn list(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        cancel: Option<&CancelToken>,
    ) -> Result<BackendListing, Error> {
        bail_if_cancelled(cancel)?;

        let (handles, filter) = self
            .resolve_listing(storage.to_ptp(), parent.map(ObjectHandle::to_ptp), cancel)
            .await?;

        let total = handles.len();
        let state = ListingState {
            session: Arc::clone(&self.session),
            handles,
            cursor: 0,
            filter,
            cancel: cancel.cloned(),
        };

        let items = futures::stream::unfold(state, |mut state| async move {
            loop {
                if state.cursor >= state.handles.len() {
                    return None;
                }

                // Cooperative cancel check before issuing the per-handle USB roundtrip. On a
                // 1k-photo listing this is the actual stop point.
                let handle = state.handles[state.cursor];

                if let Err(e) = bail_if_cancelled(state.cancel.as_ref()) {
                    return Some((
                        Err(BackendListingError::fatal(handle.into(), Error::from(e))),
                        state,
                    ));
                }

                state.cursor += 1;

                let mut info = match state.session.get_object_info_full(handle).await {
                    Ok(info) => info,
                    Err(e) => {
                        let disposition = matches!(
                            e,
                            PtpError::Protocol {
                                code: ResponseCode::GeneralError,
                                operation: OperationCode::GetObjectInfo,
                            }
                        );
                        let source = Error::from(e);
                        let error = if disposition {
                            BackendListingError::skippable(handle.into(), source)
                        } else {
                            BackendListingError::fatal(handle.into(), source)
                        };
                        return Some((Err(error), state));
                    }
                };
                info.handle = handle;

                if !state.filter.accepts(info.parent) {
                    continue;
                }

                return Some((Ok(ObjectInfo::from_ptp(info)), state));
            }
        });

        Ok(BackendListing {
            total,
            items: Box::pin(items),
        })
    }

    async fn object_info(&self, obj: ObjectHandle) -> Result<ObjectInfo, Error> {
        let mut info = self.session.get_object_info_full(obj.to_ptp()).await?;
        info.handle = obj.to_ptp();
        Ok(ObjectInfo::from_ptp(info))
    }

    async fn download(
        &self,
        obj: ObjectHandle,
        range: ByteRange,
    ) -> Result<BackendDownload, Error> {
        let info = self.session.get_object_info_full(obj.to_ptp()).await?;
        let size = info.size;
        let offset = range.offset();

        if offset > size {
            return Err(Error::invalid_data(format!(
                "download offset {offset} is past the object size {size}"
            )));
        }

        let stream = match range {
            ByteRange::Full => {
                // Whole file via GetObject (the historical fast path; no offset machinery).
                // Past 4 GiB the data container's length field is the 0xFFFFFFFF sentinel
                // (MTP 1.1 appendix H.1), so hand the resolved object size to the stream:
                // it then ends the transfer on a byte count rather than on short-packet
                // detection alone. A size still saturated at u32::MAX means
                // `get_object_info_full` couldn't resolve it, so don't pass a wrong bound.
                if size > u64::from(u32::MAX) {
                    self.session
                        .execute_with_receive_stream_sized(
                            OperationCode::GetObject,
                            &[obj.to_ptp().0],
                            size,
                        )
                        .await?
                } else {
                    self.session
                        .execute_with_receive_stream(OperationCode::GetObject, &[obj.to_ptp().0])
                        .await?
                }
            }
            ByteRange::From(_) | ByteRange::Range { .. } => {
                // Offset/range read. `max_bytes` is a u32, so a single call requests at most
                // u32::MAX bytes from the offset; a larger tail is fetched across multiple resumes.
                // Prefer GetPartialObject64 (its 64-bit offset lets a resume start past 4 GB); fall
                // back to the 32-bit GetPartialObject when that's all the device has (cameras).
                let remaining = size - offset;
                let want = match range {
                    ByteRange::Range { len, .. } => remaining.min(len),
                    _ => remaining,
                };
                let max_bytes = u32::try_from(want).unwrap_or(u32::MAX);
                match self.partial_read(offset)? {
                    PartialReadOp::Wide => {
                        let offset_lo = offset as u32;
                        let offset_hi = (offset >> 32) as u32;
                        self.session
                            .execute_with_receive_stream(
                                OperationCode::GetPartialObject64,
                                &[obj.to_ptp().0, offset_lo, offset_hi, max_bytes],
                            )
                            .await?
                    }
                    PartialReadOp::Narrow => {
                        // partial_read() guarantees offset <= u32::MAX here.
                        self.session
                            .execute_with_receive_stream(
                                OperationCode::GetPartialObject,
                                &[obj.to_ptp().0, offset as u32, max_bytes],
                            )
                            .await?
                    }
                }
            }
        };

        Ok(BackendDownload {
            // Report the full object size so a resumed/ranged download's progress/ETA stays
            // anchored to the whole file, not just this segment.
            size,
            body: Box::new(UsbDownloadBody { stream }),
        })
    }

    async fn read_range(
        &self,
        obj: ObjectHandle,
        offset: u64,
        len: Option<u32>,
    ) -> Result<Vec<u8>, Error> {
        let max_bytes = match len {
            // Whole object: GetObject buffers the lot.
            None if offset == 0 => return Ok(self.session.get_object(obj.to_ptp()).await?),
            // Tail from an offset with no explicit length: ask for as much as one call allows.
            None => u32::MAX,
            Some(len) => len,
        };
        // Prefer the 64-bit op; fall back to 32-bit GetPartialObject on cameras that only have it.
        match self.partial_read(offset)? {
            PartialReadOp::Wide => Ok(self
                .session
                .get_partial_object_64(obj.to_ptp(), offset, max_bytes)
                .await?),
            PartialReadOp::Narrow => Ok(self
                .session
                .get_partial_object(obj.to_ptp(), offset, max_bytes)
                .await?),
        }
    }

    async fn thumbnail(&self, obj: ObjectHandle) -> Result<Vec<u8>, Error> {
        Ok(self.session.get_thumb(obj.to_ptp()).await?)
    }

    async fn upload(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        info: NewObjectInfo,
        data: UploadStream<'_>,
        progress: Option<ProgressFn<'_>>,
    ) -> Result<ObjectHandle, UploadError> {
        let total_size = info.size;
        let object_info = info.to_object_info();
        let parent_handle = parent
            .map(ObjectHandle::to_ptp)
            .unwrap_or(PtpHandle::SEND_ROOT);

        // Phase 1: SendObjectInfo. No object exists yet, so a failure here has no partial to
        // surface.
        let (_, _, handle) = self
            .session
            .send_object_info(storage.to_ptp(), parent_handle, &object_info)
            .await
            .map_err(|source| UploadError {
                source: source.into(),
                partial: None,
            })?;

        // Wrap the stream to report progress and support cancellation.
        let mut bytes_sent = 0u64;
        let mut progress = progress;
        let progress_stream = data.map(move |chunk_result| {
            let chunk = chunk_result?;
            bytes_sent += chunk.len() as u64;
            if let Some(cb) = progress.as_mut() {
                let p = Progress {
                    bytes_transferred: bytes_sent,
                    total_bytes: Some(total_size),
                };
                if let ControlFlow::Break(()) = cb(p) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ));
                }
            }
            Ok(chunk)
        });

        // Phase 2: SendObject. The object already exists on the device, so any failure (genuine
        // error or cancellation) surfaces the handle as `partial` for the caller to delete or
        // resume. We do NOT delete it here.
        self.session
            .send_object_stream(total_size, progress_stream)
            .await
            .map_err(|e| match &e {
                PtpError::Io(io_err) if io_err.kind() == std::io::ErrorKind::Interrupted => {
                    Error::Cancelled
                }
                _ => Error::from(e),
            })
            .map_err(|source| UploadError {
                source,
                partial: Some(handle.into()),
            })?;

        Ok(handle.into())
    }

    async fn create_folder(
        &self,
        storage: StorageId,
        parent: Option<ObjectHandle>,
        name: &str,
    ) -> Result<ObjectHandle, Error> {
        let info = NewObjectInfo::folder(name);
        let object_info = info.to_object_info();
        let parent_handle = parent
            .map(ObjectHandle::to_ptp)
            .unwrap_or(PtpHandle::SEND_ROOT);

        let (_, _, handle) = self
            .session
            .send_object_info(storage.to_ptp(), parent_handle, &object_info)
            .await?;
        Ok(handle.into())
    }

    async fn delete(&self, obj: ObjectHandle, cancel: Option<&CancelToken>) -> Result<(), Error> {
        bail_if_cancelled(cancel)?;
        Ok(self.session.delete_object(obj.to_ptp()).await?)
    }

    async fn move_object(
        &self,
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
    ) -> Result<(), Error> {
        Ok(self
            .session
            .move_object(obj.to_ptp(), new_storage.to_ptp(), new_parent.to_ptp())
            .await?)
    }

    async fn copy_object(
        &self,
        obj: ObjectHandle,
        new_parent: ObjectHandle,
        new_storage: StorageId,
    ) -> Result<ObjectHandle, Error> {
        let handle = self
            .session
            .copy_object(obj.to_ptp(), new_storage.to_ptp(), new_parent.to_ptp())
            .await?;
        Ok(handle.into())
    }

    async fn rename(&self, obj: ObjectHandle, new_name: &str) -> Result<(), Error> {
        Ok(self.session.rename_object(obj.to_ptp(), new_name).await?)
    }

    async fn next_event(&self) -> Result<DeviceEvent, Error> {
        match self.session.poll_event().await? {
            Some(container) => Ok(DeviceEvent::from_container(&container)),
            None => Err(Error::Timeout),
        }
    }

    async fn close(&self) -> Result<(), Error> {
        // Best-effort CloseSession (the device tolerates a missing one; drop also cleans up).
        let _ = self.session.execute(OperationCode::CloseSession, &[]).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::{
        pack_u16, pack_u32, pack_u32_array, pack_u64, ContainerType, DateTime as PtpDateTime,
        DeviceInfo as PtpDeviceInfo, ObjectFormatCode, ObjectInfo as PtpObjectInfo,
    };
    use crate::transport::mock::MockTransport;

    const OVER_4GIB: u64 = u32::MAX as u64 + 1;

    #[test]
    fn a_declined_all_handle_request_falls_back_to_parent_zero() {
        // A response code is the spec'd way to decline...
        assert!(is_all_handle_rejection(&PtpError::Protocol {
            code: ResponseCode::InvalidObjectHandle,
            operation: OperationCode::GetObjectHandles,
        }));
        assert!(is_all_handle_rejection(&PtpError::Protocol {
            code: ResponseCode::OperationNotSupported,
            operation: OperationCode::GetObjectHandles,
        }));
        assert!(is_all_handle_rejection(&PtpError::Protocol {
            code: ResponseCode::InvalidParentObject,
            operation: OperationCode::GetObjectHandles,
        }));
        // ...and a bulk STALL is how SIC cameras say the same thing (#12). The
        // transport can only surface that as `Io`, so `Io` has to fall back too.
        assert!(is_all_handle_rejection(&PtpError::Io(
            std::io::Error::other("stall")
        )));
    }

    #[test]
    fn a_broken_session_propagates_instead_of_falling_back() {
        // These are the session or the transport failing, not the device
        // declining. Retrying hammers a sick device and reports the second
        // error, hiding the first.
        for err in [
            PtpError::Protocol {
                code: ResponseCode::GeneralError,
                operation: OperationCode::GetObjectHandles,
            },
            PtpError::DeviceReset,
            PtpError::Timeout,
            PtpError::Disconnected,
            PtpError::Cancelled,
            PtpError::SessionNotOpen,
            PtpError::invalid_data("desync"),
        ] {
            assert!(
                !is_all_handle_rejection(&err),
                "{err:?} must propagate, not trigger the parent=0 fallback"
            );
        }
    }

    #[test]
    fn the_storage_id_is_a_root_marker_only_while_it_isnt_an_object() {
        let storage = PtpStorageId(0x0001_0001);
        let h = |v| PtpHandle(v);

        // Nothing claims that handle, so root objects reporting it are at the root
        // (DBI/Sphaira, PR #20).
        let clean = root_filter(storage, &[h(10), h(20)]);
        assert!(clean.accepts(h(0x0001_0001)));

        // A real folder owns it, so the same parent value means "inside that
        // folder" and must not read as root.
        let collides = root_filter(storage, &[h(0x0001_0001), h(20)]);
        assert!(!collides.accepts(h(0x0001_0001)));

        // The reserved handles are never object handles, so they hold either way.
        for filter in [clean, collides] {
            assert!(filter.accepts(PtpHandle::ROOT));
            assert!(filter.accepts(PtpHandle::ALL));
            assert!(!filter.accepts(h(42)));
        }
    }

    #[test]
    fn plan_partial_read_prefers_64bit_when_available() {
        // Both ops, or 64-bit only: always Wide, at any offset.
        for offset in [0, 1024, OVER_4GIB, u64::MAX] {
            assert_eq!(
                plan_partial_read(true, true, offset).unwrap(),
                PartialReadOp::Wide
            );
            assert_eq!(
                plan_partial_read(true, false, offset).unwrap(),
                PartialReadOp::Wide
            );
        }
    }

    #[test]
    fn plan_partial_read_falls_back_to_32bit_under_4gib() {
        // 32-bit only, offset fits in u32: Narrow (the camera fallback, #12).
        for offset in [0, 1, 1024, u64::from(u32::MAX)] {
            assert_eq!(
                plan_partial_read(false, true, offset).unwrap(),
                PartialReadOp::Narrow
            );
        }
    }

    #[test]
    fn plan_partial_read_32bit_only_past_4gib_errors() {
        // 32-bit offset can't reach past 4 GiB, and there's no 64-bit op.
        for offset in [OVER_4GIB, u64::MAX] {
            assert!(matches!(
                plan_partial_read(false, true, offset),
                Err(Error::InvalidData { .. })
            ));
        }
    }

    #[test]
    fn plan_partial_read_neither_op_is_unsupported() {
        for offset in [0, 1024, OVER_4GIB] {
            assert!(matches!(
                plan_partial_read(false, false, offset),
                Err(Error::Unsupported)
            ));
        }
    }

    // -- Protocol-level mock helpers (mirror the session/storage test helpers) ----

    fn mock_transport() -> (Arc<dyn crate::transport::Transport>, Arc<MockTransport>) {
        let mock = Arc::new(MockTransport::new());
        let transport: Arc<dyn crate::transport::Transport> = Arc::clone(&mock) as _;
        (transport, mock)
    }

    fn ok_response(tx_id: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&pack_u32(12));
        buf.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
        buf.extend_from_slice(&pack_u16(ResponseCode::Ok.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        buf
    }

    fn error_response(tx_id: u32, code: ResponseCode) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.extend_from_slice(&pack_u32(12));
        buf.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
        buf.extend_from_slice(&pack_u16(code.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        buf
    }

    fn data_container(tx_id: u32, code: OperationCode, payload: &[u8]) -> Vec<u8> {
        let len = 12 + payload.len();
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(&pack_u32(len as u32));
        buf.extend_from_slice(&pack_u16(ContainerType::Data.to_code()));
        buf.extend_from_slice(&pack_u16(code.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        buf.extend_from_slice(payload);
        buf
    }

    /// Build a `UsbBackend` over a mock transport with a given vendor extension descriptor.
    /// Queues the OpenSession response; the caller queues further responses before listing.
    async fn mock_backend(
        transport: Arc<dyn crate::transport::Transport>,
        vendor_extension_desc: &str,
    ) -> UsbBackend {
        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let ptp_info = PtpDeviceInfo {
            vendor_extension_desc: vendor_extension_desc.to_string(),
            ..PtpDeviceInfo::default()
        };
        UsbBackend::new(session, ptp_info)
    }

    fn object_info_bytes(filename: &str, parent: u32) -> Vec<u8> {
        let info = PtpObjectInfo {
            storage_id: PtpStorageId(1),
            format: ObjectFormatCode::Jpeg,
            parent: PtpHandle(parent),
            filename: filename.to_string(),
            created: Some(PtpDateTime {
                year: 2024,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
            }),
            ..PtpObjectInfo::default()
        };
        info.to_bytes().unwrap()
    }

    fn object_info_bytes_with_size(filename: &str, parent: u32, size: u64) -> Vec<u8> {
        let info = PtpObjectInfo {
            storage_id: PtpStorageId(1),
            format: ObjectFormatCode::Jpeg,
            parent: PtpHandle(parent),
            filename: filename.to_string(),
            size,
            ..PtpObjectInfo::default()
        };
        info.to_bytes().unwrap()
    }

    fn queue_handles(mock: &MockTransport, tx_id: u32, handles: &[u32]) {
        let data = pack_u32_array(handles);
        mock.queue_response(data_container(
            tx_id,
            OperationCode::GetObjectHandles,
            &data,
        ));
        mock.queue_response(ok_response(tx_id));
    }

    fn queue_object_info(mock: &MockTransport, tx_id: u32, filename: &str, parent: u32) {
        let data = object_info_bytes(filename, parent);
        mock.queue_response(data_container(tx_id, OperationCode::GetObjectInfo, &data));
        mock.queue_response(ok_response(tx_id));
    }

    fn queue_object_info_with_size(
        mock: &MockTransport,
        tx_id: u32,
        filename: &str,
        parent: u32,
        size: u64,
    ) {
        let data = object_info_bytes_with_size(filename, parent, size);
        mock.queue_response(data_container(tx_id, OperationCode::GetObjectInfo, &data));
        mock.queue_response(ok_response(tx_id));
    }

    fn queue_object_size_prop(mock: &MockTransport, tx_id: u32, size: u64) {
        let payload = pack_u64(size);
        mock.queue_response(data_container(
            tx_id,
            OperationCode::GetObjectPropValue,
            &payload,
        ));
        mock.queue_response(ok_response(tx_id));
    }

    fn response_with_params(tx_id: u32, code: ResponseCode, params: &[u32]) -> Vec<u8> {
        let len = 12 + params.len() * 4;
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(&pack_u32(len as u32));
        buf.extend_from_slice(&pack_u16(ContainerType::Response.to_code()));
        buf.extend_from_slice(&pack_u16(code.into()));
        buf.extend_from_slice(&pack_u32(tx_id));
        for param in params {
            buf.extend_from_slice(&pack_u32(*param));
        }
        buf
    }

    /// The parameter list of every command container the host sent for `operation`.
    /// Asserting on the wire is the only way to catch a wrong parent constant: the
    /// mock answers `Ok` whatever we send, and so does a lenient device.
    fn command_params(mock: &MockTransport, operation: OperationCode) -> Vec<Vec<u32>> {
        let operation_code: u16 = operation.into();
        mock.get_sends()
            .into_iter()
            .filter(|send| {
                send.len() >= 12
                    && u16::from_le_bytes([send[4], send[5]]) == ContainerType::Command.to_code()
                    && u16::from_le_bytes([send[6], send[7]]) == operation_code
            })
            .map(|send| {
                send[12..]
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes(chunk.try_into().unwrap()))
                    .collect()
            })
            .collect()
    }

    /// Drive a `BackendListing` to a Vec, surfacing the first error.
    async fn collect(mut listing: BackendListing) -> Result<Vec<ObjectInfo>, Error> {
        let mut out = Vec::new();
        while let Some(item) = listing.items.next().await {
            out.push(item.map_err(|error| error.source)?);
        }
        Ok(out)
    }

    const SID: StorageId = StorageId(1);

    // -- Root listing fast-path / filtering --------------------------------------

    #[tokio::test]
    async fn list_root_fast_path_filters_non_root() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "root_file.jpg", 0); // parent=ROOT, included
        queue_object_info(&mock, 3, "nested.jpg", 99); // filtered out
        queue_object_info(&mock, 4, "another_root.txt", 0); // included

        let backend = mock_backend(transport, "").await;
        let listing = backend.list(SID, None, None).await.unwrap();
        assert_eq!(listing.total, 3);
        let objs = collect(listing).await.unwrap();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].filename, "root_file.jpg");
        assert_eq!(objs[1].filename, "another_root.txt");
    }

    #[tokio::test]
    async fn list_root_accepts_both_parent_values() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "dcim", 0); // parent=0
        queue_object_info(&mock, 3, "download", 0xFFFFFFFF); // parent=ALL
        queue_object_info(&mock, 4, "nested", 42); // not root

        let backend = mock_backend(transport, "").await;
        let objs = collect(backend.list(SID, None, None).await.unwrap())
            .await
            .unwrap();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].filename, "dcim");
        assert_eq!(objs[1].filename, "download");
    }

    #[tokio::test]
    async fn list_root_accepts_storage_id_as_parent() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20]);
        // DBI reports the containing storage ID as each root object's parent.
        queue_object_info(&mock, 2, "dbi-root", u32::try_from(SID.0).unwrap());
        queue_object_info(&mock, 3, "nested", 42); // not root

        let backend = mock_backend(transport, "").await;
        let objs = collect(backend.list(SID, None, None).await.unwrap())
            .await
            .unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].filename, "dbi-root");
    }

    #[tokio::test]
    async fn a_folder_handle_colliding_with_the_storage_id_keeps_its_children_out_of_root() {
        // The storage-ID-as-root-parent convention only makes sense while the
        // storage ID isn't ALSO a real object. Here handle 1 == SID, so parent=1
        // means "inside that folder", not "at the root", and the recursive
        // fallback would otherwise smuggle the folder's children into the root.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        // Recursive enumeration: the colliding folder plus one of its children.
        queue_handles(&mock, 1, &[1, 10, 20]);
        queue_object_info(&mock, 2, "collides-with-storage-id", 0); // real root folder
        queue_object_info(&mock, 3, "child-of-that-folder", 1); // NOT root
        queue_object_info(&mock, 4, "real-root-file", 0);

        let backend = mock_backend(transport, "").await;
        let objs = collect(backend.list(SID, None, None).await.unwrap())
            .await
            .unwrap();
        let names: Vec<_> = objs.iter().map(|o| o.filename.as_str()).collect();
        assert_eq!(names, ["collides-with-storage-id", "real-root-file"]);
    }

    #[tokio::test]
    async fn list_empty_directory() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[]);

        let backend = mock_backend(transport, "").await;
        let listing = backend.list(SID, None, None).await.unwrap();
        assert_eq!(listing.total, 0);
        assert!(collect(listing).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_subfolder_uses_exact_filter() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        let parent = 42u32;
        queue_handles(&mock, 1, &[100, 101]);
        queue_object_info(&mock, 2, "IMG_001.jpg", parent);
        queue_object_info(&mock, 3, "IMG_002.jpg", parent);

        let backend = mock_backend(transport, "").await;
        let objs = collect(
            backend
                .list(SID, Some(ObjectHandle(u64::from(parent))), None)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].filename, "IMG_001.jpg");
    }

    #[tokio::test]
    async fn list_propagates_mid_listing_error() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20]);
        queue_object_info(&mock, 2, "good.jpg", 0);
        mock.queue_response(error_response(3, ResponseCode::InvalidObjectHandle));

        let backend = mock_backend(transport, "").await;
        let mut listing = backend.list(SID, None, None).await.unwrap();
        let first = listing.items.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "good.jpg");
        assert!(listing.items.next().await.unwrap().is_err());
    }

    // -- Root as a write destination ---------------------------------------------

    #[tokio::test]
    async fn root_upload_addresses_the_storage_root_not_handle_zero() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        mock.queue_response(response_with_params(
            1,
            ResponseCode::Ok,
            &[SID.0 as u32, 0xFFFF_FFFF, 77],
        ));
        mock.queue_response(ok_response(2));

        let backend = mock_backend(transport, "").await;
        let data = futures::stream::iter(vec![Ok(Bytes::from_static(b"data"))]);
        let handle = backend
            .upload(
                SID,
                None,
                NewObjectInfo::file("file.bin", 4),
                Box::pin(data),
                None,
            )
            .await
            .unwrap();

        assert_eq!(handle, ObjectHandle(77));
        assert_eq!(
            command_params(&mock, OperationCode::SendObjectInfo),
            vec![vec![SID.0 as u32, 0xFFFF_FFFF]]
        );
    }

    #[tokio::test]
    async fn root_folder_addresses_the_storage_root_not_handle_zero() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        mock.queue_response(response_with_params(
            1,
            ResponseCode::Ok,
            &[SID.0 as u32, 0xFFFF_FFFF, 78],
        ));

        let backend = mock_backend(transport, "").await;
        let handle = backend.create_folder(SID, None, "folder").await.unwrap();

        assert_eq!(handle, ObjectHandle(78));
        assert_eq!(
            command_params(&mock, OperationCode::SendObjectInfo),
            vec![vec![SID.0 as u32, 0xFFFF_FFFF]]
        );
    }

    #[tokio::test]
    async fn a_named_write_parent_is_passed_through_untouched() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        mock.queue_response(response_with_params(
            1,
            ResponseCode::Ok,
            &[SID.0 as u32, 42, 79],
        ));

        let backend = mock_backend(transport, "").await;
        backend
            .create_folder(SID, Some(ObjectHandle(42)), "folder")
            .await
            .unwrap();

        assert_eq!(
            command_params(&mock, OperationCode::SendObjectInfo),
            vec![vec![SID.0 as u32, 42]]
        );
    }

    #[tokio::test]
    async fn move_and_copy_keep_handle_zero_for_the_root_destination() {
        // The spec is asymmetric here, so this test pins the asymmetry down:
        // `MoveObject`/`CopyObject` (D.2.25/D.2.26) spell the root destination
        // `0x00000000`, unlike `SendObjectInfo`. Don't "unify" these.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        mock.queue_response(ok_response(1));
        mock.queue_response(response_with_params(2, ResponseCode::Ok, &[99]));

        let backend = mock_backend(transport, "").await;
        backend
            .move_object(ObjectHandle(5), ObjectHandle::ROOT, SID)
            .await
            .unwrap();
        backend
            .copy_object(ObjectHandle(5), ObjectHandle::ROOT, SID)
            .await
            .unwrap();

        assert_eq!(
            command_params(&mock, OperationCode::MoveObject),
            vec![vec![5, SID.0 as u32, 0]]
        );
        assert_eq!(
            command_params(&mock, OperationCode::CopyObject),
            vec![vec![5, SID.0 as u32, 0]]
        );
    }

    // -- Tolerant collection (per-object metadata failures) ----------------------

    #[tokio::test]
    async fn list_general_error_is_tolerated_only_by_collection() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "first.jpg", 0);
        mock.queue_response(error_response(3, ResponseCode::GeneralError));
        queue_object_info(&mock, 4, "last.jpg", 0);

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());
        let collection = storage.list_objects_detailed(None).await.unwrap();

        assert_eq!(collection.objects.len(), 2);
        assert_eq!(collection.objects[0].filename, "first.jpg");
        assert_eq!(collection.objects[1].filename, "last.jpg");
        assert_eq!(collection.skipped.len(), 1);
        assert_eq!(collection.skipped[0].handle, ObjectHandle(20));
        assert!(matches!(
            collection.skipped[0].error,
            Error::Other { ref detail } if detail == "GeneralError"
        ));
    }

    #[tokio::test]
    async fn list_objects_returns_valid_siblings_around_general_error() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "first.jpg", 0);
        mock.queue_response(error_response(3, ResponseCode::GeneralError));
        queue_object_info(&mock, 4, "last.jpg", 0);

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());
        let objects = storage.list_objects(None).await.unwrap();

        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].filename, "first.jpg");
        assert_eq!(objects[1].filename, "last.jpg");
    }

    #[tokio::test]
    async fn stream_keeps_general_error_observable_and_continues() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "first.jpg", 0);
        mock.queue_response(error_response(3, ResponseCode::GeneralError));
        queue_object_info(&mock, 4, "last.jpg", 0);

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        assert_eq!(listing.next().await.unwrap().unwrap().filename, "first.jpg");
        assert!(matches!(
            listing.next().await.unwrap(),
            Err(Error::Other { ref detail }) if detail == "GeneralError"
        ));
        assert_eq!(listing.next().await.unwrap().unwrap().filename, "last.jpg");
        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn invalid_object_handle_remains_fatal_for_collection() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20]);
        queue_object_info(&mock, 2, "first.jpg", 0);
        mock.queue_response(error_response(3, ResponseCode::InvalidObjectHandle));

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());

        assert!(matches!(
            storage.list_objects_detailed(None).await,
            Err(Error::StaleHandle)
        ));
    }

    #[tokio::test]
    async fn handle_enumeration_general_error_remains_fatal() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        mock.queue_response(error_response(1, ResponseCode::GeneralError));

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());

        assert!(matches!(
            storage.list_objects_detailed(None).await,
            Err(Error::Other { ref detail }) if detail == "GeneralError"
        ));
    }

    // -- Root fallback (Samsung) -------------------------------------------------

    #[tokio::test]
    async fn list_root_falls_back_on_error() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        // Fast path (0xFFFFFFFF) rejected.
        mock.queue_response(error_response(1, ResponseCode::InvalidObjectHandle));
        // Fallback (parent=0) succeeds.
        queue_handles(&mock, 2, &[10, 20]);
        queue_object_info(&mock, 3, "root.jpg", 0);
        queue_object_info(&mock, 4, "nested.jpg", 99); // filtered by Exact(ROOT)

        let backend = mock_backend(transport, "").await;
        let objs = collect(backend.list(SID, None, None).await.unwrap())
            .await
            .unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].filename, "root.jpg");
    }

    #[tokio::test]
    async fn list_root_empty_is_not_fallback() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[]); // fast path returns empty

        let backend = mock_backend(transport, "").await;
        let listing = backend.list(SID, None, None).await.unwrap();
        assert_eq!(listing.total, 0);
    }

    // -- >4 GB size resolution ---------------------------------------------------

    #[tokio::test]
    async fn object_info_resolves_saturated_size() {
        const REAL_SIZE: u64 = 5 * 1024 * 1024 * 1024;
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_object_info_with_size(&mock, 1, "big.mkv", 0, REAL_SIZE);
        queue_object_size_prop(&mock, 2, REAL_SIZE);

        let backend = mock_backend(transport, "").await;
        let info = backend.object_info(ObjectHandle(42)).await.unwrap();
        assert_eq!(info.size, REAL_SIZE);
    }

    #[tokio::test]
    async fn object_info_skips_lookup_when_size_fits_u32() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_object_info_with_size(&mock, 1, "small.jpg", 0, 1_000_000);

        let backend = mock_backend(transport, "").await;
        let info = backend.object_info(ObjectHandle(42)).await.unwrap();
        assert_eq!(info.size, 1_000_000);
    }

    #[tokio::test]
    async fn object_info_falls_back_when_prop_lookup_fails() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_object_info_with_size(&mock, 1, "big.mkv", 0, 8 * 1024 * 1024 * 1024);
        mock.queue_response(error_response(2, ResponseCode::OperationNotSupported));

        let backend = mock_backend(transport, "").await;
        let info = backend.object_info(ObjectHandle(42)).await.unwrap();
        assert_eq!(info.size, u64::from(u32::MAX));
    }

    // -- Cancellation ------------------------------------------------------------

    #[tokio::test]
    async fn list_cancel_before_first_handle_bails() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20, 30]);

        let backend = mock_backend(transport, "").await;
        let cancel = CancelToken::new();
        let mut listing = backend.list(SID, None, Some(&cancel)).await.unwrap();
        assert_eq!(listing.total, 3);
        cancel.cancel();
        let first = listing.items.next().await.expect("expected Some(Err)");
        assert!(matches!(
            first,
            Err(BackendListingError {
                source: Error::Cancelled,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn list_cancel_mid_listing_bails_at_next_boundary() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20, 30]);
        queue_object_info(&mock, 2, "first.jpg", 0);

        let backend = mock_backend(transport, "").await;
        let cancel = CancelToken::new();
        let mut listing = backend.list(SID, None, Some(&cancel)).await.unwrap();
        let first = listing.items.next().await.unwrap().unwrap();
        assert_eq!(first.filename, "first.jpg");
        cancel.cancel();
        let second = listing.items.next().await.expect("expected Some(Err)");
        assert!(matches!(
            second,
            Err(BackendListingError {
                source: Error::Cancelled,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn delete_with_cancel_bails_before_request() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));

        let backend = mock_backend(transport, "").await;
        let cancel = CancelToken::new();
        cancel.cancel();
        let result = backend.delete(ObjectHandle(1), Some(&cancel)).await;
        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[tokio::test]
    async fn delete_no_token_runs_normally() {
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        mock.queue_response(ok_response(1)); // DeleteObject

        let backend = mock_backend(transport, "").await;
        assert!(backend.delete(ObjectHandle(1), None).await.is_ok());
    }
}
