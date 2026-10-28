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
    DeviceInfo as PtpDeviceInfo, ObjectHandle as PtpHandle, ObjectInfo as PtpObjectInfo,
    ObjectPropListElement, ObjectPropertyCode, OperationCode, PtpSession, ReceiveStream,
    ResponseCode, StorageId as PtpStorageId,
};
use crate::PtpError;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
    /// Whether the device advertises `GetObjectPropList` and the consumer didn't turn it off,
    /// which lets a folder listing skip the per-child `GetObjectInfo` (see
    /// [`UsbBackend::list_by_prop_list`]).
    has_object_prop_list: bool,
    /// Set once the device has shown it can't serve the required properties: listings go per
    /// handle for the rest of the session. Never cleared.
    prop_list_off: AtomicBool,
    /// [`PROP_LIST_OPTIONAL`] properties the device declined, as bits by position: not asked
    /// again this session.
    prop_list_declined_optional: AtomicU8,
}

impl UsbBackend {
    /// Build the backend from an open session, deriving neutral info/capabilities once.
    ///
    /// `use_object_prop_list: false` keeps every listing on the per-handle path, whatever the
    /// device advertises (see [`MtpDeviceBuilder::use_object_prop_list`]).
    ///
    /// [`MtpDeviceBuilder::use_object_prop_list`]: crate::mtp::MtpDeviceBuilder::use_object_prop_list
    pub(crate) fn new(
        session: Arc<PtpSession>,
        ptp_info: PtpDeviceInfo,
        use_object_prop_list: bool,
    ) -> Self {
        let device_info = DeviceInfo::from_ptp(&ptp_info);
        let capabilities = Capabilities::from_ptp_device_info(&ptp_info);
        let has_partial_object_64 = ptp_info.supports_operation(OperationCode::GetPartialObject64);
        let has_partial_object = ptp_info.supports_operation(OperationCode::GetPartialObject);
        let has_object_prop_list =
            use_object_prop_list && ptp_info.supports_operation(OperationCode::GetObjectPropList);
        Self {
            session,
            device_info,
            capabilities,
            has_partial_object_64,
            has_partial_object,
            has_object_prop_list,
            prop_list_off: AtomicBool::new(false),
            prop_list_declined_optional: AtomicU8::new(0),
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

    /// List `parent`'s children with `GetObjectPropList` (depth 1) instead of one `GetObjectInfo`
    /// per child, or `Ok(None)` to have the caller list per handle.
    ///
    /// One request per property: [`PROP_LIST_REQUIRED`], then [`PROP_LIST_OPTIONAL`]. Never "all
    /// properties" (`0xFFFFFFFF`): Android builds that answer from the first child's format group,
    /// so whether it succeeds and how long it takes depend on which file happens to come first,
    /// and a failing one can take seconds before it's declined. Sizes come from the 64-bit
    /// `ObjectSize` property, so files over 4 GB need no extra lookup.
    ///
    /// An answer stands in for the per-handle listing only when every child carries the required
    /// properties and names `parent` as its parent; anything else falls back rather than showing
    /// a folder with objects missing. So does an answer with no children: the per-handle path
    /// then confirms the folder is empty with one `GetObjectHandles`, so a device that answers
    /// depth 1 like depth 0 can't make a full folder look empty (which reads as "everything was
    /// deleted"). Only the device saying no and unusable answers fall back; a failing session
    /// propagates.
    ///
    /// What the device has shown it can't do sticks for the session (see
    /// [`UsbBackend::prop_list_off`] and [`UsbBackend::prop_list_declined_optional`]), so later
    /// listings waste no requests on it.
    /// A refusal about this folder alone (see [`is_prop_list_capability_decline`]) falls back
    /// for this listing only.
    async fn list_by_prop_list(
        &self,
        storage: PtpStorageId,
        parent: PtpHandle,
        cancel: Option<&CancelToken>,
    ) -> Result<Option<Vec<PtpObjectInfo>>, PtpError> {
        if self.prop_list_off.load(Ordering::Relaxed) {
            return Ok(None);
        }

        let mut elements = Vec::new();
        for property in PROP_LIST_REQUIRED {
            bail_if_cancelled(cancel)?;
            match self.ask_prop_list(parent, property).await? {
                PropListAnswer::Elements(answer) => elements.extend(answer),
                PropListAnswer::Declined | PropListAnswer::Unparseable => {
                    self.prop_list_off.store(true, Ordering::Relaxed);
                    return Ok(None);
                }
                PropListAnswer::Refused => return Ok(None),
            }
        }
        let mut children = match children_from_prop_list(&elements, storage, parent) {
            Ok(children) if children.is_empty() => return Ok(None),
            Ok(children) => children,
            Err(why) => {
                diag_debug!("GetObjectPropList per property: {why:?}");
                if why == Unusable::Systemic {
                    self.prop_list_off.store(true, Ordering::Relaxed);
                }
                return Ok(None);
            }
        };
        self.add_optional_properties(parent, &mut children, cancel)
            .await?;
        Ok(Some(children))
    }

    /// Ask for each of [`PROP_LIST_OPTIONAL`] and set it on `children`.
    ///
    /// A decline, or an answer that gives no child a value (Android answers `DateCreated` as
    /// `Undefined` for every object), is remembered for the session, so it costs one request
    /// per session rather than one per listing.
    async fn add_optional_properties(
        &self,
        parent: PtpHandle,
        children: &mut [PtpObjectInfo],
        cancel: Option<&CancelToken>,
    ) -> Result<(), PtpError> {
        let index: HashMap<PtpHandle, usize> = children
            .iter()
            .enumerate()
            .map(|(i, child)| (child.handle, i))
            .collect();
        for (bit, property) in PROP_LIST_OPTIONAL.into_iter().enumerate() {
            let bit = 1 << bit;
            if self.prop_list_declined_optional.load(Ordering::Relaxed) & bit != 0 {
                continue;
            }
            bail_if_cancelled(cancel)?;
            let served = match self.ask_prop_list(parent, property).await? {
                // Objects that arrived since the required answers aren't in `index`; they show
                // up in the next listing. Only the asked property is taken, so an answer can't
                // overwrite a required one that was already checked.
                PropListAnswer::Elements(answer) => {
                    let mut served = false;
                    for element in answer.into_iter().filter(|e| e.property == property) {
                        if let (Some(&i), Some(value)) =
                            (index.get(&element.handle), &element.value)
                        {
                            served |= children[i].set_property(element.property, value);
                        }
                    }
                    served
                }
                PropListAnswer::Declined | PropListAnswer::Unparseable => false,
                // This folder only: the date stays unset this time and is asked again next time.
                PropListAnswer::Refused => continue,
            };
            if !served {
                diag_debug!("GetObjectPropList serves no {property:?}; not asking again");
                self.prop_list_declined_optional
                    .fetch_or(bit, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// One `GetObjectPropList` request for `property` of `parent` and its children.
    ///
    /// Only the device saying no (see [`is_all_handle_rejection`], split by
    /// [`is_prop_list_capability_decline`]) and a dataset we can't parse come back as answers;
    /// everything else is the session or the transport failing, and propagates.
    async fn ask_prop_list(
        &self,
        parent: PtpHandle,
        property: ObjectPropertyCode,
    ) -> Result<PropListAnswer, PtpError> {
        match self
            .session
            .get_object_prop_list(parent, None, Some(property), 0, 1)
            .await
        {
            Ok(data) => Ok(match ObjectPropListElement::list_from_bytes(&data) {
                Ok(elements) => PropListAnswer::Elements(elements),
                Err(_e) => {
                    diag_debug!("GetObjectPropList {property:?}: unparseable dataset ({_e})");
                    PropListAnswer::Unparseable
                }
            }),
            Err(e) if is_prop_list_capability_decline(&e) => {
                diag_debug!("GetObjectPropList {property:?} declined ({e})");
                Ok(PropListAnswer::Declined)
            }
            Err(e) if is_all_handle_rejection(&e) => {
                diag_debug!("GetObjectPropList {property:?} refused for {parent:?} ({e})");
                Ok(PropListAnswer::Refused)
            }
            Err(e) => Err(e),
        }
    }
}

/// What a listing can't do without, asked first. Their order sets the listing's order: objects
/// keep the order they first appear in.
///
/// `ObjectSize` is required of files only, since a folder's size means nothing (it reads 0).
/// `StorageId` isn't asked: the listing's own storage stands in when an answer leaves it out.
const PROP_LIST_REQUIRED: [ObjectPropertyCode; 4] = [
    ObjectPropertyCode::ParentObject,
    ObjectPropertyCode::ObjectFileName,
    ObjectPropertyCode::ObjectFormat,
    ObjectPropertyCode::ObjectSize,
];

/// What a listing reads when the device serves it. A decline is tolerated and remembered (as a
/// bit of [`UsbBackend::prop_list_declined_optional`], by position here), so it costs one
/// request per session rather than one per listing.
const PROP_LIST_OPTIONAL: [ObjectPropertyCode; 2] = [
    ObjectPropertyCode::DateModified,
    ObjectPropertyCode::DateCreated,
];

/// What one `GetObjectPropList` request came to, short of a failing session.
enum PropListAnswer {
    Elements(Vec<ObjectPropListElement>),
    /// The device can't serve this request at all (see [`is_prop_list_capability_decline`]):
    /// remembered for the session.
    Declined,
    /// The device refused it for this folder (a stale handle, an unreadable object): fall back
    /// for this listing only.
    Refused,
    /// The transaction completed, so the session is fine, but the dataset doesn't parse.
    Unparseable,
}

/// Why a `GetObjectPropList` answer can't stand in for the per-handle listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unusable {
    /// The device doesn't serve what a listing needs (a required property missing from every
    /// child, or children of another folder in the answer): no use asking again this session.
    Systemic,
    /// Some children lack a required property, as when an object comes or goes between two
    /// per-property requests: fall back this time only.
    Incomplete,
}

/// Fold `GetObjectPropList` elements into `parent`'s children, one `ObjectInfo` per handle in
/// first-seen order (an object's elements are usually adjacent, but the spec doesn't promise
/// it), and check each child carries [`PROP_LIST_REQUIRED`] and names `parent` as its parent.
///
/// `parent` itself is dropped by handle, since a depth-1 answer may include it. A child of
/// another folder isn't filtered out: it means the device didn't answer what we asked, so none
/// of the answer is trusted.
fn children_from_prop_list(
    elements: &[ObjectPropListElement],
    storage: PtpStorageId,
    parent: PtpHandle,
) -> Result<Vec<PtpObjectInfo>, Unusable> {
    struct Child {
        info: PtpObjectInfo,
        /// Which of [`PROP_LIST_REQUIRED`] it carries, as bits by position.
        required: u8,
        has_storage: bool,
    }

    let mut children: Vec<Child> = Vec::new();
    let mut index = HashMap::new();
    for element in elements.iter().filter(|e| e.handle != parent) {
        let i = *index.entry(element.handle).or_insert_with(|| {
            children.push(Child {
                info: PtpObjectInfo {
                    handle: element.handle,
                    ..PtpObjectInfo::default()
                },
                required: 0,
                has_storage: false,
            });
            children.len() - 1
        });
        let child = &mut children[i];
        let Some(value) = &element.value else {
            continue;
        };
        if !child.info.set_property(element.property, value) {
            continue;
        }
        child.required |= required_bit(element.property);
        child.has_storage |= element.property == ObjectPropertyCode::StorageId;
    }

    let parent_bit = required_bit(ObjectPropertyCode::ParentObject);
    if children
        .iter()
        .any(|c| c.required & parent_bit != 0 && c.info.parent != parent)
    {
        return Err(Unusable::Systemic);
    }
    let needs = |child: &Child| {
        let all = (1 << PROP_LIST_REQUIRED.len()) - 1;
        if child.info.is_folder() {
            all & !required_bit(ObjectPropertyCode::ObjectSize)
        } else {
            all
        }
    };
    let mut verdict = Ok(());
    for property in PROP_LIST_REQUIRED {
        let bit = required_bit(property);
        let (lacking, needing) = children
            .iter()
            .filter(|c| needs(c) & bit != 0)
            .fold((0, 0), |(lacking, needing), c| {
                (lacking + usize::from(c.required & bit == 0), needing + 1)
            });
        if lacking > 0 && lacking == needing {
            return Err(Unusable::Systemic);
        }
        if lacking > 0 {
            verdict = Err(Unusable::Incomplete);
        }
    }
    verdict?;

    Ok(children
        .into_iter()
        .map(|mut child| {
            if !child.has_storage {
                child.info.storage_id = storage;
            }
            child.info
        })
        .collect())
}

/// `property`'s bit in a set of [`PROP_LIST_REQUIRED`], by position; `0` for any other.
fn required_bit(property: ObjectPropertyCode) -> u8 {
    PROP_LIST_REQUIRED
        .iter()
        .position(|&p| p == property)
        .map_or(0, |i| 1 << i)
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
/// - `Protocol`: the spec'd way to say no (`OperationNotSupported`,
///   `InvalidObjectHandle`, `InvalidParameter`, …). **Any** response code counts,
///   deliberately. Narrowing this to an allowlist looks tidier and isn't: a
///   device that declines with `AccessDenied`, `StoreNotAvailable`,
///   `InvalidStorageId`, or a vendor code then loses its root listing entirely,
///   and those are precisely the devices nobody here can test. The device
///   answered, which is the only thing that distinguishes a decline from a sick
///   session, and that's what this predicate is for.
/// - `Io`: how a bulk STALL arrives, which is how SIC-compliant cameras signal
///   an unsupported operation (Panasonic Lumix DMC-TZ61, #12). The transport
///   folds STALL in with `Fault`/`InvalidArgument`/`Unknown`, so `Io` can't be
///   narrowed further without a new `PtpError` variant; it stays on the
///   permissive side deliberately, since losing camera root listings would be
///   the worse failure.
fn is_all_handle_rejection(err: &PtpError) -> bool {
    matches!(err, PtpError::Protocol { .. } | PtpError::Io(_))
}

/// Of the refusals [`is_all_handle_rejection`] accepts, is this one a `GetObjectPropList`
/// *capability* decline: the operation, the property, or the request's shape isn't servable on
/// this device at all, so no later listing will do better?
///
/// Those demote the session's prop-list mode for good. Every other response code is about the
/// folder asked for, so it falls back for that listing only: Android re-keys handles after a
/// media rescan, so a cached folder handle answers `InvalidObjectHandle` (or
/// `InvalidParentObject`), and one partially-readable folder answers `GeneralError` (#22).
/// Treating those as capability declines would turn the fast path off for the whole session
/// over one stale handle or one bad folder.
///
/// A STALL (`Io`) counts as a capability decline: it's how SIC cameras say "unsupported
/// operation" (#12), and it carries no response code to tell otherwise.
fn is_prop_list_capability_decline(err: &PtpError) -> bool {
    match err {
        PtpError::Io(_) => true,
        PtpError::Protocol { code, .. } => {
            matches!(
                code,
                ResponseCode::OperationNotSupported | ResponseCode::ParameterNotSupported
            ) || matches!(
                u16::from(*code),
                0xA801 // Invalid_ObjectPropCode
                | 0xA802 // Invalid_ObjectProp_Format
                | 0xA807 // Specification_By_Group_Unsupported
                | 0xA808 // Specification_By_Depth_Unsupported
            )
        }
        _ => false,
    }
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

        // Root listings keep the per-handle path and its quirk handling (they're short anyway).
        if let Some(parent) = parent.filter(|_| self.has_object_prop_list) {
            if let Some(objects) = self
                .list_by_prop_list(storage.to_ptp(), parent.to_ptp(), cancel)
                .await?
            {
                let objects: Vec<_> = objects
                    .into_iter()
                    .map(|info| Ok(ObjectInfo::from_ptp(info)))
                    .collect();
                return Ok(BackendListing {
                    total: objects.len(),
                    items: Box::pin(futures::stream::iter(objects)),
                });
            }
            diag_debug!("GetObjectPropList unusable for {parent:?}; listing per handle");
        }

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
        // ...and so is any OTHER response code. These are the ones an allowlist
        // would quietly drop, on devices nobody here owns: losing their root
        // listing is a far worse outcome than one extra `parent=0` roundtrip.
        for code in [
            ResponseCode::AccessDenied,
            ResponseCode::InvalidStorageId,
            ResponseCode::StoreReadOnly,
            ResponseCode::ParameterNotSupported,
        ] {
            assert!(
                is_all_handle_rejection(&PtpError::Protocol {
                    code,
                    operation: OperationCode::GetObjectHandles,
                }),
                "{code:?} is the device declining; it must still fall back to parent=0"
            );
        }
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
        UsbBackend::new(session, ptp_info, true)
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
                offset: None,
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
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&quad| u32::from_le_bytes(quad))
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

    // -- GetObjectPropList listings ----------------------------------------------

    const FOLDER: u32 = 5;
    const PARENT: u16 = 0xDC0B;
    const NAME: u16 = 0xDC07;
    const FORMAT: u16 = 0xDC02;
    const SIZE: u16 = 0xDC04;
    const MODIFIED: u16 = 0xDC09;
    const CREATED: u16 = 0xDC08;
    /// What a listing asks, in order: what it can't do without, then dates.
    const PER_PROPERTY: [u16; 6] = [PARENT, NAME, FORMAT, SIZE, MODIFIED, CREATED];
    const REQUIRED: [u16; 4] = [PARENT, NAME, FORMAT, SIZE];

    /// A `(handle, property, data type, value)` element of a `GetObjectPropList` dataset.
    type Element = (u32, u16, u16, Vec<u8>);

    /// A backend whose device advertises `GetObjectPropList`.
    async fn mock_backend_with_prop_list(
        transport: Arc<dyn crate::transport::Transport>,
    ) -> UsbBackend {
        let session = Arc::new(PtpSession::open(transport, 1).await.unwrap());
        let ptp_info = PtpDeviceInfo {
            operations_supported: vec![OperationCode::GetObjectPropList],
            ..PtpDeviceInfo::default()
        };
        UsbBackend::new(session, ptp_info, true)
    }

    /// A `GetObjectPropList` dataset of `(handle, property, data type, value)` elements.
    fn prop_list(elements: &[Element]) -> Vec<u8> {
        let mut buf = pack_u32(elements.len() as u32).to_vec();
        for (handle, property, data_type, value) in elements {
            buf.extend_from_slice(&pack_u32(*handle));
            buf.extend_from_slice(&pack_u16(*property));
            buf.extend_from_slice(&pack_u16(*data_type));
            buf.extend_from_slice(value);
        }
        buf
    }

    fn queue_prop_list(mock: &MockTransport, tx_id: u32, elements: &[Element]) {
        mock.queue_response(data_container(
            tx_id,
            OperationCode::GetObjectPropList,
            &prop_list(elements),
        ));
        mock.queue_response(ok_response(tx_id));
    }

    /// The per-property answer for `property` in `FOLDER`: the folder itself (depth 1 includes
    /// it), then each `(handle, name)` child, a 3-byte JPEG.
    fn answer(property: u16, children: &[(u32, &str)]) -> Vec<Element> {
        std::iter::once((FOLDER, "DCIM", 1))
            .chain(
                children
                    .iter()
                    .map(|&(handle, name)| (handle, name, FOLDER)),
            )
            .map(|(handle, name, parent)| {
                let is_folder = handle == FOLDER;
                let (data_type, value) = match property {
                    PARENT => (0x0006, pack_u32(parent).to_vec()),
                    NAME => (0xFFFF, crate::ptp::pack_string(name).unwrap()),
                    FORMAT => (
                        0x0004,
                        pack_u16(if is_folder { 0x3001 } else { 0x3801 }).to_vec(),
                    ),
                    SIZE => (0x0008, pack_u64(if is_folder { 0 } else { 3 }).to_vec()),
                    MODIFIED | CREATED => {
                        (0xFFFF, crate::ptp::pack_string("20260809T190833").unwrap())
                    }
                    _ => unreachable!("no test property {property:#06x}"),
                };
                (handle, property, data_type, value)
            })
            .collect()
    }

    /// Queues the device's answers in transaction order (`OpenSession` was transaction 0).
    struct Answers<'a> {
        mock: &'a MockTransport,
        tx: u32,
    }

    impl<'a> Answers<'a> {
        fn new(mock: &'a MockTransport) -> Self {
            mock.queue_response(ok_response(0));
            Answers { mock, tx: 1 }
        }

        fn next(&mut self) -> u32 {
            self.tx += 1;
            self.tx - 1
        }

        fn prop_list(&mut self, elements: &[Element]) {
            let tx = self.next();
            queue_prop_list(self.mock, tx, elements);
        }

        /// Declined with `InvalidObjectPropCode` (0xA801).
        fn decline(&mut self) {
            self.refuse(ResponseCode::from(0xA801));
        }

        /// Answered with `code` instead of data.
        fn refuse(&mut self, code: ResponseCode) {
            let tx = self.next();
            self.mock.queue_response(error_response(tx, code));
        }

        fn per_property(&mut self, properties: &[u16], children: &[(u32, &str)]) {
            for &property in properties {
                self.prop_list(&answer(property, children));
            }
        }

        /// The per-handle path: `GetObjectHandles`, then one `GetObjectInfo` per child.
        fn per_handle(&mut self, children: &[(u32, &str)]) {
            let tx = self.next();
            let handles: Vec<u32> = children.iter().map(|&(handle, _)| handle).collect();
            queue_handles(self.mock, tx, &handles);
            for &(_, name) in children {
                let tx = self.next();
                queue_object_info(self.mock, tx, name, FOLDER);
            }
        }
    }

    async fn list_folder(
        backend: &UsbBackend,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<ObjectInfo>, Error> {
        collect(
            backend
                .list(SID, Some(ObjectHandle(u64::from(FOLDER))), cancel)
                .await?,
        )
        .await
    }

    fn names(objects: &[ObjectInfo]) -> Vec<&str> {
        objects.iter().map(|o| o.filename.as_str()).collect()
    }

    /// The property parameter of every `GetObjectPropList` the host sent.
    fn prop_list_requests(mock: &MockTransport) -> Vec<u32> {
        command_params(mock, OperationCode::GetObjectPropList)
            .into_iter()
            .map(|params| params[2])
            .collect()
    }

    fn per_property_requests(properties: &[u16]) -> Vec<u32> {
        properties.iter().map(|&p| u32::from(p)).collect()
    }

    #[tokio::test]
    async fn list_subfolder_uses_object_prop_list_when_advertised() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let s = |v: &str| crate::ptp::pack_string(v).unwrap();
        let u32v = |v: u32| pack_u32(v).to_vec();
        // Depth 1 answers the folder itself (handle 5) and its children, one property at a time.
        device.prop_list(&[
            (5, PARENT, 0x0006, u32v(0)),
            (10, PARENT, 0x0006, u32v(5)),
            (11, PARENT, 0x0006, u32v(5)),
        ]);
        device.prop_list(&[
            (5, NAME, 0xFFFF, s("DCIM")),
            (10, NAME, 0xFFFF, s("Camera")),
            (11, NAME, 0xFFFF, s("big.mp4")),
        ]);
        device.prop_list(&[
            (10, FORMAT, 0x0004, pack_u16(0x3001).to_vec()),
            (11, FORMAT, 0x0004, pack_u16(0x300B).to_vec()),
        ]);
        device.prop_list(&[(11, SIZE, 0x0008, pack_u64(OVER_4GIB).to_vec())]);
        device.decline(); // DateModified
        device.decline(); // DateCreated

        let backend = mock_backend_with_prop_list(transport).await;
        let objs = list_folder(&backend, None).await.unwrap();
        assert_eq!(
            names(&objs),
            ["Camera", "big.mp4"],
            "the parent itself is left out"
        );
        assert!(
            objs[0].is_folder(),
            "the Association format, and a folder needs no size"
        );
        assert_eq!(objs[1].size, OVER_4GIB);
        assert!(
            objs.iter().all(|o| o.storage_id == SID),
            "a StorageId the answer left out is the listed storage"
        );
        // One request per property, never "all properties" (0xFFFFFFFF), and no GetObjectInfo.
        let expected: Vec<Vec<u32>> = PER_PROPERTY
            .iter()
            .map(|&p| vec![5, 0, u32::from(p), 0, 1])
            .collect();
        assert_eq!(
            command_params(&mock, OperationCode::GetObjectPropList),
            expected
        );
        assert!(command_params(&mock, OperationCode::GetObjectInfo).is_empty());
    }

    #[tokio::test]
    async fn every_listing_asks_one_property_at_a_time() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        device.per_property(&PER_PROPERTY, &[(12, "a.jpg")]);
        device.per_property(&PER_PROPERTY, &[(12, "a.jpg")]);

        let backend = mock_backend_with_prop_list(transport).await;
        for _ in 0..2 {
            let objs = list_folder(&backend, None).await.unwrap();
            assert_eq!(names(&objs), ["a.jpg"]);
            assert_eq!(objs[0].size, 3);
            assert_eq!(objs[0].storage_id, SID);
            assert!(objs[0].modified.is_some() && objs[0].created.is_some());
        }
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&[PER_PROPERTY, PER_PROPERTY].concat())
        );
        assert!(command_params(&mock, OperationCode::GetObjectInfo).is_empty());
    }

    #[tokio::test]
    async fn the_listing_keeps_the_devices_order() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        // The first answer sets the order; later ones may list the same objects differently.
        device.prop_list(&answer(PARENT, &[(12, "b.jpg"), (11, "a.jpg")]));
        device.per_property(&PER_PROPERTY[1..], &[(11, "a.jpg"), (12, "b.jpg")]);

        let backend = mock_backend_with_prop_list(transport).await;
        let objs = list_folder(&backend, None).await.unwrap();
        assert_eq!(names(&objs), ["b.jpg", "a.jpg"]);
    }

    #[tokio::test]
    async fn a_declined_required_property_lists_per_handle_and_stops_asking() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let children = [(11, "a.jpg"), (12, "b.jpg")];
        device.prop_list(&answer(PARENT, &children));
        device.decline(); // ObjectFileName
        device.per_handle(&children);
        // The device has shown it can't serve a listing: the next one goes straight per handle.
        device.per_handle(&children);

        let backend = mock_backend_with_prop_list(transport).await;
        for _ in 0..2 {
            let objs = list_folder(&backend, None).await.unwrap();
            assert_eq!(names(&objs), ["a.jpg", "b.jpg"]);
        }
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&[PARENT, NAME])
        );
    }

    #[tokio::test]
    async fn a_genuinely_empty_folder_lists_empty() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        // Depth 1 answers only the folder itself...
        device.per_property(&REQUIRED, &[]);
        // ...which the handle list confirms.
        device.per_handle(&[]);

        let backend = mock_backend_with_prop_list(transport).await;
        assert!(list_folder(&backend, None).await.unwrap().is_empty());
        assert_eq!(
            command_params(&mock, OperationCode::GetObjectHandles).len(),
            1
        );
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&REQUIRED),
            "no dates for nothing"
        );
    }

    #[tokio::test]
    async fn an_empty_prop_list_does_not_hide_a_folders_contents() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        // A device that answers depth 1 like depth 0 (the folder alone) reads as an empty
        // folder, which a file manager shows as "everything was deleted". The handle list
        // knows better.
        device.per_property(&REQUIRED, &[]);
        device.per_handle(&[(11, "a.jpg")]);

        let backend = mock_backend_with_prop_list(transport).await;
        let objs = list_folder(&backend, None).await.unwrap();
        assert_eq!(names(&objs), ["a.jpg"]);
    }

    #[tokio::test]
    async fn children_without_a_parent_fall_back_instead_of_reading_empty() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let children = [(11, "a.jpg"), (12, "b.jpg")];
        // `ParentObject` answered for the folder alone: the device doesn't serve it.
        device.prop_list(&answer(PARENT, &[]));
        device.per_property(&REQUIRED[1..], &children);
        device.per_handle(&children);
        device.per_handle(&children);

        let backend = mock_backend_with_prop_list(transport).await;
        for _ in 0..2 {
            let objs = list_folder(&backend, None).await.unwrap();
            assert_eq!(names(&objs), ["a.jpg", "b.jpg"]);
        }
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&REQUIRED),
            "no prop list for the second listing, and no dates for the first"
        );
    }

    #[tokio::test]
    async fn a_child_missing_its_name_falls_back_this_time_only() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let children = [(11, "a.jpg"), (12, "b.jpg")];
        // An object that came or went between two per-property requests: one answer lacks it.
        device.prop_list(&answer(PARENT, &children));
        device.prop_list(&answer(NAME, &children[..1]));
        device.per_property(&REQUIRED[2..], &children);
        device.per_handle(&children);
        // The device didn't prove it can't serve a listing, so the next one asks again.
        device.per_property(&PER_PROPERTY, &children);

        let backend = mock_backend_with_prop_list(transport).await;
        for _ in 0..2 {
            let objs = list_folder(&backend, None).await.unwrap();
            assert_eq!(names(&objs), ["a.jpg", "b.jpg"]);
        }
        assert_eq!(command_params(&mock, OperationCode::GetObjectInfo).len(), 2);
    }

    #[tokio::test]
    async fn a_child_of_another_folder_is_not_silently_filtered_out() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let children = [(11, "a.jpg"), (12, "b.jpg")];
        // A depth-1 answer naming an object somewhere else: the device didn't answer what we
        // asked, so none of its answer is trusted.
        let mut parents = answer(PARENT, &children);
        parents[2].3 = pack_u32(99).to_vec();
        device.prop_list(&parents);
        device.per_property(&REQUIRED[1..], &children);
        device.per_handle(&children);
        device.per_handle(&children);

        let backend = mock_backend_with_prop_list(transport).await;
        for _ in 0..2 {
            let objs = list_folder(&backend, None).await.unwrap();
            assert_eq!(names(&objs), ["a.jpg", "b.jpg"]);
        }
        assert_eq!(prop_list_requests(&mock), per_property_requests(&REQUIRED));
    }

    #[tokio::test]
    async fn a_declined_date_is_tolerated_and_not_asked_again() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let children = [(11, "a.jpg")];
        device.per_property(&PER_PROPERTY[..5], &children);
        device.decline(); // DateCreated
        device.per_property(&PER_PROPERTY[..5], &children);

        let backend = mock_backend_with_prop_list(transport).await;
        for _ in 0..2 {
            let objs = list_folder(&backend, None).await.unwrap();
            assert_eq!(names(&objs), ["a.jpg"]);
            assert!(objs[0].modified.is_some());
            assert_eq!(objs[0].created, None);
        }
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&[&PER_PROPERTY[..], &PER_PROPERTY[..5]].concat())
        );
        assert!(command_params(&mock, OperationCode::GetObjectInfo).is_empty());
    }

    /// `property` with data type `Undefined` and no value for the folder and each child: how
    /// Android answers a property it doesn't serve, such as `DateCreated`.
    fn undefined_answer(property: u16, children: &[(u32, &str)]) -> Vec<Element> {
        std::iter::once(FOLDER)
            .chain(children.iter().map(|&(handle, _)| handle))
            .map(|handle| (handle, property, 0x0000, Vec::new()))
            .collect()
    }

    #[tokio::test]
    async fn a_date_served_for_nothing_is_not_asked_again() {
        // Android answers DateCreated alone as Undefined for every object.
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let children = [(11, "a.jpg")];
        device.per_property(&PER_PROPERTY[..5], &children);
        device.prop_list(&undefined_answer(CREATED, &children));
        device.per_property(&PER_PROPERTY[..5], &children);

        let backend = mock_backend_with_prop_list(transport).await;
        for _ in 0..2 {
            let objs = list_folder(&backend, None).await.unwrap();
            assert!(objs[0].modified.is_some());
            assert_eq!(objs[0].created, None);
        }
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&[&PER_PROPERTY[..], &PER_PROPERTY[..5]].concat())
        );
    }

    #[tokio::test]
    async fn a_stale_folder_handle_falls_back_this_listing_only() {
        // Android re-keys handles: a folder handle cached before a media rescan answers
        // InvalidObjectHandle. That says nothing about whether the device lists by prop list.
        for code in [
            ResponseCode::InvalidObjectHandle,
            ResponseCode::InvalidParentObject,
            ResponseCode::GeneralError,
            ResponseCode::AccessDenied,
            ResponseCode::DeviceBusy,
        ] {
            let (transport, mock) = mock_transport();
            let mut device = Answers::new(&mock);
            let children = [(11, "a.jpg")];
            device.refuse(code);
            device.per_handle(&children);
            device.per_property(&PER_PROPERTY, &children);

            let backend = mock_backend_with_prop_list(transport).await;
            for _ in 0..2 {
                let objs = list_folder(&backend, None).await.unwrap();
                assert_eq!(names(&objs), ["a.jpg"], "{code:?}");
            }
            assert_eq!(
                prop_list_requests(&mock),
                per_property_requests(&[&[PARENT][..], &PER_PROPERTY[..]].concat()),
                "{code:?}: the next listing asks again"
            );
        }
    }

    #[tokio::test]
    async fn a_folder_refusing_a_required_property_keeps_prop_list_on() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let children = [(11, "a.jpg")];
        device.per_property(&PER_PROPERTY[..1], &children);
        device.refuse(ResponseCode::GeneralError); // ObjectFileName, for this folder only
        device.per_handle(&children);
        device.per_property(&PER_PROPERTY, &children);

        let backend = mock_backend_with_prop_list(transport).await;
        for _ in 0..2 {
            let objs = list_folder(&backend, None).await.unwrap();
            assert_eq!(names(&objs), ["a.jpg"]);
        }
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&[&PER_PROPERTY[..2], &PER_PROPERTY[..]].concat())
        );
    }

    #[tokio::test]
    async fn a_folder_refusing_a_date_is_asked_again_next_listing() {
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        let children = [(11, "a.jpg")];
        device.per_property(&PER_PROPERTY[..5], &children);
        device.refuse(ResponseCode::GeneralError);
        device.per_property(&PER_PROPERTY, &children);

        let backend = mock_backend_with_prop_list(transport).await;
        let first = list_folder(&backend, None).await.unwrap();
        assert_eq!(first[0].created, None);
        let second = list_folder(&backend, None).await.unwrap();
        assert!(second[0].created.is_some());
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&[PER_PROPERTY, PER_PROPERTY].concat())
        );
    }

    #[tokio::test]
    async fn capability_declines_turn_prop_list_off_for_the_session() {
        for code in [0x2005, 0x2006, 0xA801, 0xA802, 0xA807, 0xA808] {
            let (transport, mock) = mock_transport();
            let mut device = Answers::new(&mock);
            let children = [(11, "a.jpg")];
            device.refuse(ResponseCode::from(code));
            device.per_handle(&children);
            device.per_handle(&children);

            let backend = mock_backend_with_prop_list(transport).await;
            for _ in 0..2 {
                let objs = list_folder(&backend, None).await.unwrap();
                assert_eq!(names(&objs), ["a.jpg"], "{code:#06x}");
            }
            assert_eq!(
                prop_list_requests(&mock),
                per_property_requests(&[PARENT]),
                "{code:#06x}"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_session_propagates_instead_of_listing_per_handle() {
        // Nothing answers: the transport fails under the first request.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        let backend = mock_backend_with_prop_list(transport).await;
        assert!(list_folder(&backend, None).await.is_err());
        assert!(command_params(&mock, OperationCode::GetObjectHandles).is_empty());

        // A reply to an earlier transaction mid-way: the session is out of step, which surfaces
        // as `InvalidData` like an unparseable dataset does, but must not be mistaken for one.
        let (transport, mock) = mock_transport();
        let mut device = Answers::new(&mock);
        device.prop_list(&answer(PARENT, &[(11, "a.jpg")]));
        mock.queue_response(ok_response(1));
        let backend = mock_backend_with_prop_list(transport).await;
        assert!(list_folder(&backend, None).await.is_err());
        assert!(command_params(&mock, OperationCode::GetObjectHandles).is_empty());
    }

    /// Hands everything to the mock, and cancels `token` once the host has sent `after`
    /// `GetObjectPropList` commands: someone cancelling while a listing is under way.
    struct CancelAfterPropLists {
        mock: Arc<MockTransport>,
        token: CancelToken,
        after: usize,
        sent: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl crate::transport::Transport for CancelAfterPropLists {
        async fn send_bulk(&self, data: &[u8]) -> Result<(), PtpError> {
            let is_prop_list = data.len() >= 8
                && u16::from_le_bytes([data[4], data[5]]) == ContainerType::Command.to_code()
                && u16::from_le_bytes([data[6], data[7]])
                    == u16::from(OperationCode::GetObjectPropList);
            if is_prop_list && self.sent.fetch_add(1, Ordering::SeqCst) + 1 == self.after {
                self.token.cancel();
            }
            self.mock.send_bulk(data).await
        }

        async fn receive_bulk(&self, max_size: usize) -> Result<Vec<u8>, PtpError> {
            self.mock.receive_bulk(max_size).await
        }

        async fn receive_interrupt(&self) -> Result<Vec<u8>, PtpError> {
            self.mock.receive_interrupt().await
        }

        async fn cancel_transfer(
            &self,
            transaction_id: u32,
            idle_timeout: Duration,
        ) -> Result<(), PtpError> {
            self.mock
                .cancel_transfer(transaction_id, idle_timeout)
                .await
        }
    }

    #[tokio::test]
    async fn a_cancel_between_property_requests_stops_the_listing() {
        let mock = Arc::new(MockTransport::new());
        let mut device = Answers::new(&mock);
        device.per_property(&PER_PROPERTY, &[(11, "a.jpg")]);
        let token = CancelToken::new();
        let transport = Arc::new(CancelAfterPropLists {
            mock: Arc::clone(&mock),
            token: token.clone(),
            after: 2,
            sent: Default::default(),
        });

        let backend = mock_backend_with_prop_list(transport).await;
        let result = list_folder(&backend, Some(&token)).await;
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert_eq!(
            prop_list_requests(&mock),
            per_property_requests(&[PARENT, NAME]),
            "nothing after the cancel"
        );
        assert!(command_params(&mock, OperationCode::GetObjectHandles).is_empty());
    }

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
        let collection = storage.collect_objects(None).await.unwrap();

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
    async fn stream_reports_a_skip_as_an_item_not_an_error_and_continues() {
        // The streaming half of the contract: a skippable per-object failure is a
        // successful item of kind `Skipped`, so `Err` keeps its single meaning
        // ("the listing is over") and a consumer can't confuse the two.
        use crate::mtp::ListingItem;

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

        let first = listing.next().await.unwrap().unwrap();
        assert!(matches!(first, ListingItem::Object(ref o) if o.filename == "first.jpg"));

        let skipped = listing.next().await.unwrap().unwrap();
        let ListingItem::Skipped(skipped) = skipped else {
            panic!("expected a Skipped item, got {skipped:?}");
        };
        assert_eq!(skipped.handle, ObjectHandle(20));
        assert!(matches!(
            skipped.error,
            Error::Other { ref detail } if detail == "GeneralError"
        ));

        let last = listing.next().await.unwrap().unwrap();
        assert!(matches!(last, ListingItem::Object(ref o) if o.filename == "last.jpg"));
        assert!(listing.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_still_reports_a_fatal_error_as_an_error() {
        // The other half: a non-skippable failure must not arrive as an item.
        use crate::mtp::ListingItem;

        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20]);
        queue_object_info(&mock, 2, "first.jpg", 0);
        mock.queue_response(error_response(3, ResponseCode::InvalidObjectHandle));

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());
        let mut listing = storage.list_objects_stream(None).await.unwrap();

        assert!(matches!(
            listing.next().await.unwrap().unwrap(),
            ListingItem::Object(_)
        ));
        assert!(matches!(
            listing.next().await.unwrap(),
            Err(Error::StaleHandle)
        ));
    }

    #[tokio::test]
    async fn a_folder_where_every_object_was_skipped_fails_instead_of_reading_as_empty() {
        // Tolerating one bad apple is the point; reporting a dead device as an
        // empty folder is not. `Ok(vec![])` here would render as "empty folder" in
        // a file manager and as "everything was deleted" to anything syncing.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20]);
        mock.queue_response(error_response(2, ResponseCode::GeneralError));
        mock.queue_response(error_response(3, ResponseCode::GeneralError));

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());

        assert!(matches!(
            storage.collect_objects(None).await,
            Err(Error::Other { ref detail }) if detail == "GeneralError"
        ));
    }

    #[tokio::test]
    async fn the_all_skipped_rule_also_covers_the_plain_list_objects_wrapper() {
        // `list_objects` is the one nearly every consumer calls, and it's the one
        // that would hand back an innocent-looking empty Vec.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[10, 20]);
        mock.queue_response(error_response(2, ResponseCode::GeneralError));
        mock.queue_response(error_response(3, ResponseCode::GeneralError));

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());

        assert!(matches!(
            storage.list_objects(None).await,
            Err(Error::Other { ref detail }) if detail == "GeneralError"
        ));
    }

    #[tokio::test]
    async fn a_genuinely_empty_folder_is_still_empty_not_an_error() {
        // The all-skipped rule keys on "we learned nothing about a folder we know
        // has contents". No handles means no contents, which is not a failure.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        queue_handles(&mock, 1, &[]);

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());
        let collection = storage.collect_objects(None).await.unwrap();

        assert!(collection.objects.is_empty());
        assert!(collection.skipped.is_empty());
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
            storage.collect_objects(None).await,
            Err(Error::StaleHandle)
        ));
    }

    #[tokio::test]
    async fn handle_enumeration_general_error_remains_fatal() {
        // Skippability applies to per-object metadata only. Failing to enumerate the
        // handles means we never learned what the folder contains, so there are no
        // "valid siblings" to preserve and nothing to be tolerant about.
        let (transport, mock) = mock_transport();
        mock.queue_response(ok_response(0));
        // A root listing tries `parent=0xFFFFFFFF` first and treats any response code
        // as a decline (see `is_all_handle_rejection`), so the `parent=0` fallback has
        // to fail too before the error is the listing's final answer.
        mock.queue_response(error_response(1, ResponseCode::GeneralError));
        mock.queue_response(error_response(2, ResponseCode::GeneralError));

        let backend = mock_backend(transport, "").await;
        let storage =
            crate::mtp::Storage::new(Arc::new(backend), SID, crate::mtp::StorageInfo::default());

        assert!(matches!(
            storage.collect_objects(None).await,
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
