//! `GetObjectPropList` (0x9805) for the virtual device, modeled on Android's
//! `MtpDatabase.getObjectPropertyList` (`frameworks/base`, `media/java/android/mtp/`).

use super::builders::{
    build_data_container, build_response, object_facts, pack_device_string, pack_file_time,
    ObjectFacts,
};
use super::handlers::description_refusal;
use super::state::VirtualDeviceState;
use crate::ptp::{pack_u16, pack_u32, pack_u64, ObjectHandle, ObjectPropertyCode};

const OK: u16 = 0x2001;
const GENERAL_ERROR: u16 = 0x2002;
const PARAMETER_NOT_SUPPORTED: u16 = 0x2006;
const INVALID_OBJECT_HANDLE: u16 = 0x2009;
const INVALID_OBJECT_PROP_CODE: u16 = 0xA801;
const SPECIFICATION_BY_GROUP_UNSUPPORTED: u16 = 0xA807;
const SPECIFICATION_BY_DEPTH_UNSUPPORTED: u16 = 0xA808;

/// The `property` (and `depth`) parameter meaning "all of them".
const ALL: u32 = 0xFFFF_FFFF;

const PERSISTENT_UID: u16 = 0xDC41;
const DISPLAY_NAME: u16 = 0xDCE0;
const DATE_ADDED: u16 = 0xDC4E;

/// Android's `FILE_PROPERTIES`, in its order: what "all properties" answers. No
/// `DateCreated`, which Android serves for no object.
const FILE_PROPERTIES: [u16; 11] = [
    0xDC01, // StorageID
    0xDC02, // ObjectFormat
    0xDC03, // ProtectionStatus
    0xDC04, // ObjectSize
    0xDC07, // ObjectFileName
    0xDC09, // DateModified
    PERSISTENT_UID,
    0xDC0B, // ParentObject
    0xDC44, // Name
    DISPLAY_NAME,
    DATE_ADDED,
];

/// Answer `GetObjectPropList(handle, format, property, group, depth)`.
pub(super) fn handle_get_object_prop_list(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    params: &[u32],
) {
    match answer(state, params) {
        Ok(payload) => {
            state
                .response_queue
                .push_back(build_data_container(op_code, tx_id, &payload));
            state
                .response_queue
                .push_back(build_response(OK, tx_id, &[]));
        }
        Err(code) => state
            .response_queue
            .push_back(build_response(code, tx_id, &[])),
    }
}

/// The dataset for `params`, or the response code Android would refuse them with.
fn answer(state: &mut VirtualDeviceState, params: &[u32]) -> Result<Vec<u8>, u16> {
    let param = |i: usize| params.get(i).copied().unwrap_or(0);
    let (mut handle, format, property, group, mut depth) =
        (param(0), param(1), param(2), param(3), param(4));

    // Property 0 asks by group, which Android doesn't implement.
    if property == 0 {
        return Err(if group == 0 {
            PARAMETER_NOT_SUPPORTED
        } else {
            SPECIFICATION_BY_GROUP_UNSUPPORTED
        });
    }
    if depth == ALL && (handle == 0 || handle == ALL) {
        handle = ALL;
        depth = 0;
    }
    if depth > 1 {
        return Err(SPECIFICATION_BY_DEPTH_UNSUPPORTED);
    }
    let declined = |code: u16| {
        state
            .config
            .prop_list_declined_properties
            .contains(&ObjectPropertyCode::from(code))
    };
    let properties: Vec<u16> = if property == ALL {
        FILE_PROPERTIES
            .into_iter()
            .filter(|&p| !declined(p))
            .collect()
    } else {
        let code = property as u16;
        if declined(code) {
            return Err(INVALID_OBJECT_PROP_CODE);
        }
        vec![code]
    };

    let handles = objects(state, handle, depth)?;
    let mut objects = Vec::with_capacity(handles.len());
    for handle in handles {
        if let Some(code) = description_refusal(state, handle) {
            return Err(code);
        }
        // A file that vanished since it was tracked has nothing left to describe.
        let Some(facts) = object_facts(handle, state) else {
            continue;
        };
        if format == 0 || u32::from(facts.format) == format {
            objects.push((handle, facts));
        }
    }

    let mut buf = Vec::new();
    let count = (objects.len() * properties.len()) as u32;
    buf.extend_from_slice(&pack_u32(count));
    for (handle, facts) in &objects {
        for &property in &properties {
            push_element(&mut buf, *handle, property, facts, state);
        }
    }
    Ok(buf)
}

/// The objects a request covers, in Android's order: the children first (for depth 1 or the
/// root), then the object itself.
fn objects(
    state: &mut VirtualDeviceState,
    handle: u32,
    depth: u32,
) -> Result<Vec<ObjectHandle>, u16> {
    let storages: Vec<_> = state.storages.iter().map(|s| s.storage_id).collect();
    let mut handles = Vec::new();
    if handle == ALL && depth == 0 {
        for storage in storages {
            handles.extend(state.scan_all(storage).map_err(|_| GENERAL_ERROR)?);
        }
        return Ok(handles);
    }
    if handle == 0 || handle == ALL {
        for storage in storages {
            let root = state.scan_dir(storage, ObjectHandle::ROOT);
            handles.extend(root.map_err(|_| GENERAL_ERROR)?);
        }
        return Ok(handles);
    }
    let storage = state
        .objects
        .get(&handle)
        .ok_or(INVALID_OBJECT_HANDLE)?
        .storage_id;
    if depth == 1 {
        let children = state.scan_dir(storage, ObjectHandle(handle));
        handles.extend(children.map_err(|_| GENERAL_ERROR)?);
    }
    handles.push(ObjectHandle(handle));
    Ok(handles)
}

/// One `(handle, property, data type, value)` element; a property the device doesn't know is
/// data type `Undefined` with no value, as on Android.
fn push_element(
    buf: &mut Vec<u8>,
    handle: ObjectHandle,
    property: u16,
    facts: &ObjectFacts,
    state: &VirtualDeviceState,
) {
    use ObjectPropertyCode as P;

    const UINT16: u16 = 0x0004;
    const UINT32: u16 = 0x0006;
    const UINT64: u16 = 0x0008;
    const UINT128: u16 = 0x000A;
    const STRING: u16 = 0xFFFF;

    let (data_type, value) = match property {
        PERSISTENT_UID => {
            // Unique and stable for the object's life, like Android's (path hash, mtime).
            let uid = (u128::from(facts.storage_id.0) << 32) | u128::from(handle.0);
            (UINT128, uid.to_le_bytes().to_vec())
        }
        DISPLAY_NAME => (STRING, pack_device_string(&facts.filename)),
        DATE_ADDED => (STRING, pack_file_time(facts.modified, state)),
        code => match P::from(code) {
            P::StorageId => (UINT32, pack_u32(facts.storage_id.0).to_vec()),
            P::ObjectFormat => (UINT16, pack_u16(facts.format).to_vec()),
            P::ProtectionStatus => (UINT16, pack_u16(0).to_vec()),
            P::ObjectSize => (UINT64, pack_u64(facts.size).to_vec()),
            P::ObjectFileName | P::Name => (STRING, pack_device_string(&facts.filename)),
            P::DateCreated => (STRING, pack_file_time(facts.created, state)),
            P::DateModified => (STRING, pack_file_time(facts.modified, state)),
            P::ParentObject => (UINT32, pack_u32(facts.parent.0).to_vec()),
            P::Unknown(_) => (0x0000, Vec::new()),
        },
    };
    buf.extend_from_slice(&pack_u32(handle.0));
    buf.extend_from_slice(&pack_u16(property));
    buf.extend_from_slice(&pack_u16(data_type));
    buf.extend_from_slice(&value);
}

#[cfg(test)]
mod tests {
    use super::super::config::{VirtualDeviceConfig, VirtualStorageConfig};
    use super::super::handlers::dispatch;
    use super::super::state::VirtualDeviceState;
    use crate::ptp::{
        DeviceInfo, ObjectHandle, ObjectInfo as PtpObjectInfo, ObjectPropListElement,
        ObjectPropertyCode, OperationCode, PropertyDataType, PropertyValue,
    };
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    const ALL_PROPERTIES: u32 = 0xFFFF_FFFF;
    const ROOT_ALIAS: u32 = 0xFFFF_FFFF;
    const OK: u16 = 0x2001;
    const GENERAL_ERROR: u16 = 0x2002;
    const OPERATION_NOT_SUPPORTED: u16 = 0x2005;
    const PARAMETER_NOT_SUPPORTED: u16 = 0x2006;
    const INVALID_OBJECT_HANDLE: u16 = 0x2009;
    const ACCESS_DENIED: u16 = 0x200F;
    const INVALID_OBJECT_PROP_CODE: u16 = 0xA801;
    const SPECIFICATION_BY_GROUP_UNSUPPORTED: u16 = 0xA807;
    const SPECIFICATION_BY_DEPTH_UNSUPPORTED: u16 = 0xA808;

    /// A tree of `a.txt`, `Photos/{b.jpg, Sub/}`, and `Empty/`, with fixed modified times.
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), b"abc").unwrap();
        std::fs::create_dir_all(root.join("Photos/Sub")).unwrap();
        std::fs::write(root.join("Photos/b.jpg"), b"jpeg!").unwrap();
        std::fs::create_dir(root.join("Empty")).unwrap();
        for (i, rel) in ["a.txt", "Photos/b.jpg"].into_iter().enumerate() {
            let file = std::fs::File::options()
                .write(true)
                .open(root.join(rel))
                .unwrap();
            file.set_modified(UNIX_EPOCH + Duration::from_secs(1_710_513_022 + i as u64))
                .unwrap();
        }
        dir
    }

    fn open_state(dir: &Path, tweak: impl FnOnce(&mut VirtualDeviceConfig)) -> VirtualDeviceState {
        let mut config = VirtualDeviceConfig {
            storages: vec![VirtualStorageConfig {
                description: "Internal Storage".into(),
                capacity: 1 << 30,
                backing_dir: dir.to_path_buf(),
                read_only: false,
            }],
            watch_backing_dirs: false,
            event_poll_interval: Duration::ZERO,
            ..Default::default()
        };
        tweak(&mut config);
        let mut state = VirtualDeviceState::new(config);
        state.session_open = true;
        state
    }

    /// The handle of `rel_path`, scanning the folders on the way so they're tracked.
    fn handle_of(state: &mut VirtualDeviceState, rel_path: &str) -> u32 {
        let storage = state.storages[0].storage_id;
        let mut parent = ObjectHandle::ROOT;
        let mut path = PathBuf::new();
        for part in rel_path.split('/') {
            path.push(part);
            state.scan_dir(storage, parent).unwrap();
            parent = state
                .objects
                .iter()
                .find(|(_, o)| o.rel_path == path)
                .map(|(&h, _)| ObjectHandle(h))
                .unwrap_or_else(|| panic!("{rel_path} not found"));
        }
        parent.0
    }

    /// Dispatch one operation: its data payload, or the response code when it isn't OK.
    fn run(
        state: &mut VirtualDeviceState,
        op: OperationCode,
        params: &[u32],
    ) -> Result<Vec<u8>, u16> {
        state.response_queue.clear();
        dispatch(state, op.into(), 9, params, None);
        let response = state.response_queue.pop_back().expect("a response");
        let code = u16::from_le_bytes([response[6], response[7]]);
        if code != OK {
            assert!(
                state.response_queue.is_empty(),
                "no data phase with an error"
            );
            return Err(code);
        }
        Ok(state
            .response_queue
            .pop_front()
            .map(|data| data[12..].to_vec())
            .unwrap_or_default())
    }

    fn prop_list(
        state: &mut VirtualDeviceState,
        handle: u32,
        property: u32,
        depth: u32,
    ) -> Result<Vec<ObjectPropListElement>, u16> {
        let data = run(
            state,
            OperationCode::GetObjectPropList,
            &[handle, 0, property, 0, depth],
        )?;
        Ok(ObjectPropListElement::list_from_bytes(&data).expect("a well-formed dataset"))
    }

    fn code(property: ObjectPropertyCode) -> u32 {
        u32::from(u16::from(property))
    }

    fn object_info(state: &mut VirtualDeviceState, handle: u32) -> PtpObjectInfo {
        let data = run(state, OperationCode::GetObjectInfo, &[handle]).unwrap();
        PtpObjectInfo::from_bytes(&data).unwrap()
    }

    /// Handles in the order the elements first name them.
    fn handles(elements: &[ObjectPropListElement]) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for e in elements {
            if !out.contains(&e.handle.0) {
                out.push(e.handle.0);
            }
        }
        out
    }

    fn sorted(mut handles: Vec<u32>) -> Vec<u32> {
        handles.sort_unstable();
        handles
    }

    /// Fold one object's elements into an `ObjectInfo`, as a host would.
    fn fold(elements: &[ObjectPropListElement], handle: u32) -> PtpObjectInfo {
        let mut info = PtpObjectInfo::default();
        for e in elements.iter().filter(|e| e.handle.0 == handle) {
            if let Some(value) = &e.value {
                info.set_property(e.property, value);
            }
        }
        info
    }

    #[test]
    fn prop_list_is_advertised_by_default_and_refused_when_turned_off() {
        let dir = tree();
        let advertised = |state: &mut VirtualDeviceState| {
            let data = run(state, OperationCode::GetDeviceInfo, &[]).unwrap();
            DeviceInfo::from_bytes(&data)
                .unwrap()
                .supports_operation(OperationCode::GetObjectPropList)
        };

        let mut state = open_state(dir.path(), |_| {});
        assert!(advertised(&mut state));
        let photos = handle_of(&mut state, "Photos");
        assert!(prop_list(&mut state, photos, ALL_PROPERTIES, 1).is_ok());

        let mut state = open_state(dir.path(), |c| c.supports_object_prop_list = false);
        assert!(!advertised(&mut state));
        let photos = handle_of(&mut state, "Photos");
        assert_eq!(
            prop_list(&mut state, photos, ALL_PROPERTIES, 1),
            Err(OPERATION_NOT_SUPPORTED)
        );
    }

    #[test]
    fn depth_one_lists_the_children_then_the_folder_itself() {
        let dir = tree();
        let mut state = open_state(dir.path(), |_| {});
        let photos = handle_of(&mut state, "Photos");
        let b = handle_of(&mut state, "Photos/b.jpg");
        let sub = handle_of(&mut state, "Photos/Sub");

        let listed = handles(&prop_list(&mut state, photos, ALL_PROPERTIES, 1).unwrap());
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[2], photos, "Android appends the folder itself last");
        assert_eq!(sorted(listed[..2].to_vec()), sorted(vec![b, sub]));

        // Depth 0 is the object alone.
        let listed = handles(&prop_list(&mut state, photos, ALL_PROPERTIES, 0).unwrap());
        assert_eq!(listed, [photos]);
        // A file has no children.
        let listed = handles(&prop_list(&mut state, b, ALL_PROPERTIES, 1).unwrap());
        assert_eq!(listed, [b]);
        // An empty folder answers with itself only.
        let empty = handle_of(&mut state, "Empty");
        let listed = handles(&prop_list(&mut state, empty, ALL_PROPERTIES, 1).unwrap());
        assert_eq!(listed, [empty]);
    }

    #[test]
    fn all_properties_is_androids_file_property_set_in_its_order() {
        let dir = tree();
        let mut state = open_state(dir.path(), |_| {});
        let b = handle_of(&mut state, "Photos/b.jpg");

        let elements = prop_list(&mut state, b, ALL_PROPERTIES, 0).unwrap();
        let codes: Vec<u16> = elements.iter().map(|e| e.property.into()).collect();
        assert_eq!(
            codes,
            [
                0xDC01, 0xDC02, 0xDC03, 0xDC04, 0xDC07, 0xDC09, 0xDC41, 0xDC0B, 0xDC44, 0xDCE0,
                0xDC4E
            ],
            "no DateCreated: Android's FILE_PROPERTIES leaves it out"
        );
        let size = elements
            .iter()
            .find(|e| e.property == ObjectPropertyCode::ObjectSize)
            .unwrap();
        assert_eq!(size.data_type, PropertyDataType::Uint64);
        assert_eq!(size.value, Some(PropertyValue::Uint64(5)));
    }

    #[test]
    fn prop_list_values_match_get_object_info() {
        let dir = tree();
        let mut state = open_state(dir.path(), |c| {
            c.utc_offset = crate::mtp::UtcOffset::from_minutes(120).unwrap();
            c.dates_include_offset = true;
        });
        for rel in ["a.txt", "Photos", "Photos/b.jpg", "Photos/Sub", "Empty"] {
            let handle = handle_of(&mut state, rel);
            let expected = object_info(&mut state, handle);

            let mut elements = prop_list(&mut state, handle, ALL_PROPERTIES, 0).unwrap();
            // "All" has no DateCreated; asked alone, the device serves it.
            let created = code(ObjectPropertyCode::DateCreated);
            elements.extend(prop_list(&mut state, handle, created, 0).unwrap());
            let got = fold(&elements, handle);

            assert_eq!(got.storage_id, expected.storage_id, "{rel}");
            assert_eq!(got.format, expected.format, "{rel}");
            assert_eq!(got.parent, expected.parent, "{rel}");
            assert_eq!(got.filename, expected.filename, "{rel}");
            assert_eq!(got.size, expected.size, "{rel}");
            assert_eq!(got.modified, expected.modified, "{rel}");
            assert_eq!(got.created, expected.created, "{rel}");
            assert!(got.modified.is_some(), "{rel}");
        }
        let photos = handle_of(&mut state, "Photos");
        let elements = prop_list(&mut state, photos, ALL_PROPERTIES, 0).unwrap();
        assert_eq!(
            fold(&elements, photos).parent,
            ObjectHandle::ROOT,
            "a root object's parent is 0, as on Android"
        );
    }

    #[test]
    fn object_size_is_the_full_64_bit_size() {
        const REAL_SIZE: u64 = 5 * 1024 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("movie.mkv"))
            .unwrap()
            .set_len(REAL_SIZE)
            .unwrap();
        let mut state = open_state(dir.path(), |_| {});
        let movie = handle_of(&mut state, "movie.mkv");

        let size = code(ObjectPropertyCode::ObjectSize);
        let elements = prop_list(&mut state, movie, size, 0).unwrap();
        assert_eq!(elements[0].value, Some(PropertyValue::Uint64(REAL_SIZE)));
    }

    #[test]
    fn one_property_answers_that_property_for_every_object() {
        let dir = tree();
        let mut state = open_state(dir.path(), |_| {});
        let photos = handle_of(&mut state, "Photos");

        let name = code(ObjectPropertyCode::ObjectFileName);
        let elements = prop_list(&mut state, photos, name, 1).unwrap();
        assert_eq!(elements.len(), 3);
        let mut names: Vec<_> = elements
            .iter()
            .map(|e| {
                assert_eq!(e.property, ObjectPropertyCode::ObjectFileName);
                match &e.value {
                    Some(PropertyValue::String(s)) => s.clone(),
                    other => panic!("expected a name, got {other:?}"),
                }
            })
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["Photos", "Sub", "b.jpg"]);
    }

    #[test]
    fn an_unknown_property_comes_back_undefined_without_a_value() {
        let dir = tree();
        let mut state = open_state(dir.path(), |_| {});
        let photos = handle_of(&mut state, "Photos");

        let elements = prop_list(&mut state, photos, 0xDC99, 1).unwrap();
        assert_eq!(elements.len(), 3);
        assert!(elements
            .iter()
            .all(|e| e.data_type == PropertyDataType::Undefined && e.value.is_none()));
    }

    #[test]
    fn requests_android_refuses_are_refused() {
        let dir = tree();
        let mut state = open_state(dir.path(), |_| {});
        let photos = handle_of(&mut state, "Photos");
        let op = OperationCode::GetObjectPropList;

        // Property 0 means "by group": group 0 is a bad parameter, any other group unsupported.
        assert_eq!(
            run(&mut state, op, &[photos, 0, 0, 0, 1]),
            Err(PARAMETER_NOT_SUPPORTED)
        );
        assert_eq!(
            run(&mut state, op, &[photos, 0, 0, 1, 1]),
            Err(SPECIFICATION_BY_GROUP_UNSUPPORTED)
        );
        assert_eq!(
            prop_list(&mut state, photos, ALL_PROPERTIES, 2),
            Err(SPECIFICATION_BY_DEPTH_UNSUPPORTED)
        );
        assert_eq!(
            prop_list(&mut state, 0x7777, ALL_PROPERTIES, 1),
            Err(INVALID_OBJECT_HANDLE)
        );
    }

    #[test]
    fn the_root_lists_by_either_alias() {
        let dir = tree();
        let mut state = open_state(dir.path(), |_| {});
        let root: Vec<u32> = ["a.txt", "Photos", "Empty"]
            .into_iter()
            .map(|rel| handle_of(&mut state, rel))
            .collect();
        let root = sorted(root);
        let name = code(ObjectPropertyCode::ObjectFileName);

        for alias in [0, ROOT_ALIAS] {
            let listed = handles(&prop_list(&mut state, alias, name, 1).unwrap());
            assert_eq!(sorted(listed), root, "handle {alias:#x}, depth 1");
        }
        // Handle 0 at depth 0 is the root's children too; 0xFFFFFFFF at depth 0 is everything,
        // and so is "every depth" from either alias.
        let listed = handles(&prop_list(&mut state, 0, name, 0).unwrap());
        assert_eq!(sorted(listed), root);
        assert_eq!(
            handles(&prop_list(&mut state, ROOT_ALIAS, name, 0).unwrap()).len(),
            5
        );
        assert_eq!(
            handles(&prop_list(&mut state, 0, name, 0xFFFF_FFFF).unwrap()).len(),
            5
        );
    }

    #[test]
    fn a_declined_property_is_refused_alone_and_left_out_of_all() {
        let dir = tree();
        let mut state = open_state(dir.path(), |c| {
            c.prop_list_declined_properties = vec![ObjectPropertyCode::DateModified];
        });
        let photos = handle_of(&mut state, "Photos");

        let modified = code(ObjectPropertyCode::DateModified);
        assert_eq!(
            prop_list(&mut state, photos, modified, 1),
            Err(INVALID_OBJECT_PROP_CODE)
        );
        let all = prop_list(&mut state, photos, ALL_PROPERTIES, 1).unwrap();
        assert!(!all.is_empty());
        assert!(all
            .iter()
            .all(|e| e.property != ObjectPropertyCode::DateModified));
    }

    #[test]
    fn an_undescribable_object_fails_any_answer_that_would_include_it() {
        let dir = tree();
        let mut state = open_state(dir.path(), |c| {
            c.undescribable_objects = vec!["Photos/b.jpg".into()];
        });
        let photos = handle_of(&mut state, "Photos");
        let b = handle_of(&mut state, "Photos/b.jpg");
        let sub = handle_of(&mut state, "Photos/Sub");
        let name = code(ObjectPropertyCode::ObjectFileName);

        assert_eq!(
            prop_list(&mut state, photos, ALL_PROPERTIES, 1),
            Err(GENERAL_ERROR)
        );
        assert_eq!(prop_list(&mut state, photos, name, 1), Err(GENERAL_ERROR));
        assert_eq!(prop_list(&mut state, b, name, 0), Err(GENERAL_ERROR));
        // Answers that leave it out are fine.
        assert!(prop_list(&mut state, photos, name, 0).is_ok());
        assert!(prop_list(&mut state, sub, name, 1).is_ok());

        // The runtime hook does the same, with the code it was armed with.
        let mut state = open_state(dir.path(), |_| {});
        let photos = handle_of(&mut state, "Photos");
        let b = handle_of(&mut state, "Photos/b.jpg");
        state.forced_object_info_errors.insert(b, ACCESS_DENIED);
        assert_eq!(prop_list(&mut state, photos, name, 1), Err(ACCESS_DENIED));
    }
}
