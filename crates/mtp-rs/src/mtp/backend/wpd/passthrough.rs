//! Real partial reads on WPD through the Windows MTP driver's passthrough (#61).
//!
//! WPD's resource `IStream` is forward-only on Android (`Seek` is `E_NOTIMPL`), so reopening it at
//! an offset re-reads the whole prefix. The MTP class driver (WpdMtpDr) also lets an app send raw
//! MTP operations with `IPortableDevice::SendCommand` and the `WPD_COMMAND_MTP_EXT_*` commands, so
//! a window of a paced download, or a `read_range`, can be one `GetPartialObject64` (0x95C1) or
//! `GetPartialObject` (0x101B) transaction: no stream to keep, so other requests between windows
//! cost nothing. Research and hardware numbers: `docs/specs/wpd-partial-reads.md`.
//!
//! One transaction is `EXECUTE_COMMAND_WITH_DATA_TO_READ` (returns a transfer context and the data
//! size), `READ_DATA` until the data is in, then `END_DATA_TRANSFER` (returns the MTP response). A
//! started data phase is always read to the end before `END`: an early `END` is the driver's own
//! cancel, which on Android may hit the `io_cancel` race that #54 removed `CLASS_CANCEL` for.
//!
//! The driver runs these inside its own MTP session and numbers the transactions itself. Never
//! send `OpenSession`/`CloseSession` or a write through here (writes would bypass the driver's
//! object cache).

use super::props::{map_hresult, take_pwstr, wide};
use crate::mtp::error::map_response_code;
use crate::mtp::{Error, ObjectHandle};
use crate::ptp::{OperationCode, ResponseCode};
use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::{Error as WinError, HRESULT, PCWSTR};
use windows::Win32::Devices::PortableDevices::*;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Variant::VT_UI4;

const OP_GET_DEVICE_INFO: u16 = 0x1001;
const OP_GET_OBJECT_INFO: u16 = 0x1008;
const OP_GET_PARTIAL_OBJECT: u16 = 0x101B;
const OP_GET_PARTIAL_OBJECT_64: u16 = 0x95C1;
/// `FILE_READ_ACCESS | FILE_WRITE_ACCESS` (`winioctl.h`), for `WPD_API_OPTION_IOCTL_ACCESS`.
const IOCTL_READ_WRITE: u32 = 0x1 | 0x2;
const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
/// `READ_DATA` size when the driver names no optimal size. The Pixel's driver says 256 KiB, and the
/// chunk size made no difference to throughput there (spec § Results).
const DEFAULT_CHUNK: u32 = 256 * 1024;
/// The data size the driver reports when it doesn't know it: read until a short chunk.
const UNKNOWN_TOTAL: u64 = u32::MAX as u64;

/// The `WPD_COMMAND_MTP_EXT_*` commands a partial read needs. The driver lists them in
/// `GetSupportedCommands` when it offers the passthrough.
pub(crate) const REQUIRED_COMMANDS: [PROPERTYKEY; 3] = [
    WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ,
    WPD_COMMAND_MTP_EXT_READ_DATA,
    WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER,
];

/// Which partial-read operation a read uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartialOp {
    /// `GetPartialObject64` (0x95C1): 64-bit offset.
    Wide,
    /// `GetPartialObject` (0x101B): 32-bit offset, for devices without the 64-bit op.
    Narrow,
}

impl PartialOp {
    fn opcode(self) -> u16 {
        match self {
            PartialOp::Wide => OP_GET_PARTIAL_OBJECT_64,
            PartialOp::Narrow => OP_GET_PARTIAL_OBJECT,
        }
    }

    /// The operation's parameters for `len` bytes of MTP object `handle` from `offset`. The caller
    /// picked `Narrow` only for offsets that fit in 32 bits.
    fn params(self, handle: u32, offset: u64, len: u32) -> Vec<u32> {
        match self {
            PartialOp::Wide => vec![handle, offset as u32, (offset >> 32) as u32, len],
            PartialOp::Narrow => vec![handle, offset as u32, len],
        }
    }
}

/// The partial-read operations the device advertises, from its `DeviceInfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PartialOps {
    wide: bool,
    narrow: bool,
}

impl PartialOps {
    /// The operation for a read starting at `offset`, if any covers it.
    fn for_offset(self, offset: u64) -> Option<PartialOp> {
        if self.wide {
            Some(PartialOp::Wide)
        } else if self.narrow && offset <= u64::from(u32::MAX) {
            Some(PartialOp::Narrow)
        } else {
            None
        }
    }
}

/// The MTP object handle in a WpdMtpDr object id: `o<hex>`, like `oEF` for handle 0xEF. An
/// undocumented driver detail (seen on a Pixel, and what WpdMtpLib parses), so anything else means
/// "use the stream".
pub(crate) fn mtp_handle_from_wpd_id(wpd_id: &str) -> Option<u32> {
    let hex = wpd_id.strip_prefix('o')?;
    if hex.is_empty() || hex.len() > 8 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// Whether an MTP `ObjectInfo` (name, 32-bit size) describes the same file as WPD's properties.
/// The dataset's size saturates at `u32::MAX` for files of 4 GiB and more.
fn same_object(mtp_name: &str, mtp_size: u64, wpd_name: &str, wpd_size: u64) -> bool {
    let size_ok = mtp_size == wpd_size
        || (mtp_size == u64::from(u32::MAX) && wpd_size >= u64::from(u32::MAX));
    size_ok && mtp_name == wpd_name
}

/// A started data-to-read transaction.
pub(crate) struct Transfer {
    context: Vec<u16>,
    /// Bytes in the data phase; [`UNKNOWN_TOTAL`] when the driver doesn't know.
    total: u64,
    /// Bytes per `READ_DATA`.
    chunk: u32,
}

impl Transfer {
    /// Bytes in the data phase, or `None` when the driver doesn't know (read to a short chunk).
    pub(crate) fn total(&self) -> Option<u64> {
        (self.total != UNKNOWN_TOTAL).then_some(self.total)
    }

    /// Whether the transaction is already over: a read with no data, which
    /// [`Passthrough::begin_partial`] ends straight away to check the device's response.
    fn ended(&self) -> bool {
        self.total == 0
    }
}

/// The `READ_DATA` size for the next call.
fn next_ask(t: &Transfer, got: u64) -> u64 {
    match t.total() {
        Some(total) => total.saturating_sub(got).min(u64::from(t.chunk)),
        None => u64::from(t.chunk),
    }
}

/// Whether a data phase is over after a chunk of `last_chunk` bytes (of `asked`), with `got`
/// bytes in so far. With an unknown total, a short chunk ends it.
fn data_done(t: &Transfer, got: u64, last_chunk: usize, asked: u64) -> bool {
    last_chunk == 0
        || match t.total() {
            Some(total) => got >= total,
            None => (last_chunk as u64) < asked,
        }
}

/// How [`Passthrough::finish`] went.
pub(crate) struct Finished {
    /// Bytes the device sent.
    pub(crate) bytes: u64,
    /// The sink stopped wanting chunks (the rest was read and dropped).
    pub(crate) consumer_stopped: bool,
    /// Time in `READ_DATA` calls, without the time `sink` took: what the pacer sizes windows by.
    pub(crate) device_time: std::time::Duration,
    /// Time in the data phase, `sink` included (zero without the `tracing` feature).
    pub(crate) data_time: std::time::Duration,
    /// Time in `END_DATA_TRANSFER` (zero without the `tracing` feature).
    pub(crate) end_time: std::time::Duration,
    /// A failed read, or a response other than OK.
    pub(crate) result: Result<(), Error>,
}

/// How a read through the passthrough went, for the caller's choice between it and the stream.
pub(crate) enum Refusal {
    /// The passthrough can't serve this read (or any read: then it's off for the session). Use the
    /// stream; nothing reached the device's data phase.
    UseStream,
    /// A failure the stream wouldn't fix (the device is gone).
    Fatal(Error),
}

/// The MTP passthrough of one open device, with what it learned about it this session.
pub(crate) struct Passthrough {
    device: IPortableDevice,
    /// Set once a `SendCommand` came back `E_ACCESSDENIED` and the retry with an explicit IOCTL
    /// access level worked, so later commands send it straight away.
    ioctl_access: bool,
    /// The device's partial-read operations: `None` until the first read asks (one passthrough
    /// `GetDeviceInfo`), so an open costs nothing extra.
    ops: Option<PartialOps>,
    /// Off for the rest of the session: the driver or device declined the passthrough.
    off: bool,
    /// MTP handle per object, checked once with a passthrough `GetObjectInfo` against WPD's name
    /// and size (`None`: the id doesn't map, use the stream). A wrong mapping would read another
    /// file's bytes, so nothing is trusted unchecked.
    handles: HashMap<ObjectHandle, Option<u32>>,
}

impl Passthrough {
    /// The passthrough for `device` when its driver lists every command a partial read needs.
    pub(crate) fn new(device: &IPortableDevice, supported: &[PROPERTYKEY]) -> Option<Self> {
        let listed = REQUIRED_COMMANDS.iter().all(|k| {
            supported
                .iter()
                .any(|c| c.fmtid == k.fmtid && c.pid == k.pid)
        });
        diag_debug!("wpd: MTP passthrough commands listed: {}", listed);
        listed.then(|| Self {
            device: device.clone(),
            ioctl_access: false,
            ops: None,
            off: false,
            handles: HashMap::new(),
        })
    }

    /// The operation for a read starting at `offset`, learning the device's operations on the
    /// first call. `None` means the stream serves the read.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn op_for(&mut self, offset: u64) -> Option<PartialOp> {
        if self.off {
            return None;
        }
        let ops = match self.ops {
            Some(ops) => ops,
            None => {
                let ops = self.probe_ops();
                self.ops = Some(ops);
                ops
            }
        };
        ops.for_offset(offset)
    }

    /// Ask the device's `DeviceInfo` which partial-read operations it has. A failure turns the
    /// passthrough off for the session.
    unsafe fn probe_ops(&mut self) -> PartialOps {
        let timer = crate::trace::DiagTimer::start();
        let result = self
            .read_whole(OP_GET_DEVICE_INFO, &[])
            .and_then(|(rc, data)| {
                if rc != ResponseCode::Ok {
                    return Err(map_response_code(rc));
                }
                crate::ptp::DeviceInfo::from_bytes(&data).map_err(Error::from)
            });
        match result {
            Ok(info) => {
                let ops = PartialOps {
                    wide: info.supports_operation(OperationCode::GetPartialObject64),
                    narrow: info.supports_operation(OperationCode::GetPartialObject),
                };
                diag_debug!(
                    "wpd: passthrough GetDeviceInfo in {:?}: GetPartialObject64 {}, GetPartialObject {}",
                    timer.elapsed(),
                    ops.wide,
                    ops.narrow
                );
                ops
            }
            Err(e) => {
                diag_debug!(
                    "wpd: passthrough GetDeviceInfo failed in {:?}, using streams: {}",
                    timer.elapsed(),
                    e
                );
                self.off = true;
                PartialOps::default()
            }
        }
    }

    /// Whether `obj`'s MTP handle was already checked (so [`Self::handle_for`] needs no WPD name
    /// and size for it).
    pub(crate) fn has_checked(&self, obj: ObjectHandle) -> bool {
        self.handles.contains_key(&obj)
    }

    /// The checked MTP handle of `obj` (WPD id `wpd_id`, name and size from WPD), or `None` when
    /// the stream has to serve it. Checked once per object per session.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn handle_for(
        &mut self,
        obj: ObjectHandle,
        wpd_id: &str,
        wpd_name: &str,
        wpd_size: u64,
    ) -> Option<u32> {
        if let Some(known) = self.handles.get(&obj) {
            return *known;
        }
        let checked = mtp_handle_from_wpd_id(wpd_id).filter(|&h| {
            let timer = crate::trace::DiagTimer::start();
            let result = self
                .read_whole(OP_GET_OBJECT_INFO, &[h])
                .and_then(|(rc, data)| {
                    if rc != ResponseCode::Ok {
                        return Err(map_response_code(rc));
                    }
                    crate::ptp::ObjectInfo::from_bytes(&data).map_err(Error::from)
                });
            let ok = match &result {
                Ok(info) => same_object(&info.filename, info.size, wpd_name, wpd_size),
                Err(_) => false,
            };
            diag_debug!(
                "wpd: passthrough handle {:#x} for {} checked in {:?}: {}",
                h,
                wpd_id,
                timer.elapsed(),
                match &result {
                    Ok(_) if ok => "same object".to_string(),
                    Ok(info) => format!(
                        "different object ({:?}, {} bytes; WPD says {:?}, {} bytes)",
                        info.filename, info.size, wpd_name, wpd_size
                    ),
                    Err(e) => format!("failed: {e}"),
                }
            );
            ok
        });
        if checked.is_none() {
            diag_debug!(
                "wpd: {} has no checked MTP handle, using the stream",
                wpd_id
            );
        }
        self.handles.insert(obj, checked);
        checked
    }

    /// Start a partial read of `len` bytes of `handle` from `offset`.
    ///
    /// A device that refuses the operation skips the data phase, so with no data the transaction
    /// is ended here and its response checked: the caller then either has an empty read (`Ok`
    /// with a total of 0) or a [`Refusal`].
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn begin_partial(
        &mut self,
        op: PartialOp,
        handle: u32,
        offset: u64,
        len: u32,
    ) -> Result<Transfer, Refusal> {
        let t = match self.begin(op.opcode(), &op.params(handle, offset, len)) {
            Ok(t) => t,
            Err(e) => return Err(self.refusal(e)),
        };
        if t.total == 0 {
            let rc = self.end(&t).map_err(|e| self.refusal(e))?;
            if rc != ResponseCode::Ok {
                return Err(self.refusal(map_response_code(rc)));
            }
        }
        Ok(t)
    }

    /// What a failure to start a read means: a capability decline turns the passthrough off for
    /// the session, a gone device is fatal, and anything else sends this read to the stream.
    fn refusal(&mut self, e: Error) -> Refusal {
        diag_debug!("wpd: passthrough partial read refused: {}", e);
        match e {
            Error::Disconnected => Refusal::Fatal(e),
            Error::Unsupported | Error::AccessDenied => {
                self.off = true;
                Refusal::UseStream
            }
            _ => Refusal::UseStream,
        }
    }

    /// `EXECUTE_COMMAND_WITH_DATA_TO_READ`: send `opcode` and start its data phase.
    ///
    /// # Safety
    /// COM thread only.
    unsafe fn begin(&mut self, opcode: u16, params: &[u32]) -> Result<Transfer, Error> {
        let p = command(&WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ)?;
        p.SetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_OPERATION_CODE, u32::from(opcode))
            .map_err(map_hresult)?;
        let coll = u32_collection(params)?;
        p.SetIPortableDevicePropVariantCollectionValue(
            &WPD_PROPERTY_MTP_EXT_OPERATION_PARAMS,
            &coll,
        )
        .map_err(map_hresult)?;
        let r = self.send(&p)?;
        let context = take_pwstr(
            r.GetStringValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT)
                .map_err(map_hresult)?,
        );
        // The reference page says VT_UI8; Microsoft's own sample reads it as a ULONG.
        let total =
            match r.GetUnsignedLargeIntegerValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE) {
                Ok(v) => v,
                Err(_) => r
                    .GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE)
                    .map(u64::from)
                    .map_err(map_hresult)?,
            };
        let optimal = r
            .GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_OPTIMAL_TRANSFER_BUFFER_SIZE)
            .unwrap_or(0);
        Ok(Transfer {
            context: wide(&context),
            total,
            chunk: if optimal > 0 { optimal } else { DEFAULT_CHUNK },
        })
    }

    /// `READ_DATA`: the next `n` (at most `t.chunk`, see [`Self::next_ask`]) bytes of the data
    /// phase. `buf` is the input buffer the driver insists on; it is never written to.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn read(
        &mut self,
        t: &Transfer,
        n: u64,
        buf: &mut Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        let n = n.min(u64::from(t.chunk)) as u32;
        buf.resize(n as usize, 0);
        let p = command(&WPD_COMMAND_MTP_EXT_READ_DATA)?;
        p.SetStringValue(
            &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT,
            PCWSTR(t.context.as_ptr()),
        )
        .map_err(map_hresult)?;
        p.SetBufferValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_DATA, buf)
            .map_err(map_hresult)?;
        p.SetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_READ, n)
            .map_err(map_hresult)?;
        let r = self.send(&p)?;
        take_buffer(&r, &WPD_PROPERTY_MTP_EXT_TRANSFER_DATA)
    }

    /// Read the rest of a started data phase to its end, handing each chunk to `sink`, then end
    /// the transaction. The data phase is read to the end even when `sink` stops wanting chunks
    /// (returns `false`): ending early is the driver's own cancel (module docs).
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn finish(
        &mut self,
        t: &Transfer,
        mut sink: impl FnMut(Vec<u8>) -> bool,
    ) -> Finished {
        let mut done = Finished {
            bytes: 0,
            consumer_stopped: false,
            device_time: std::time::Duration::ZERO,
            data_time: std::time::Duration::ZERO,
            end_time: std::time::Duration::ZERO,
            result: Ok(()),
        };
        if t.ended() {
            return done;
        }
        let timer = crate::trace::DiagTimer::start();
        let mut buf = Vec::new();
        let read_result = loop {
            if t.total() == Some(done.bytes) {
                break Ok(());
            }
            let ask = next_ask(t, done.bytes);
            let reading = std::time::Instant::now();
            let chunk = self.read(t, ask, &mut buf);
            done.device_time += reading.elapsed();
            match chunk {
                Ok(c) => {
                    let n = c.len();
                    done.bytes += n as u64;
                    if !done.consumer_stopped && !sink(c) {
                        done.consumer_stopped = true;
                    }
                    if data_done(t, done.bytes, n, ask) {
                        break Ok(());
                    }
                }
                Err(e) => break Err(e),
            }
        };
        done.data_time = timer.elapsed();
        // End the transaction even after a failed read, so the driver closes it.
        let timer = crate::trace::DiagTimer::start();
        let ended = self.end(t);
        done.end_time = timer.elapsed();
        done.result = read_result.and_then(|()| match ended? {
            ResponseCode::Ok => Ok(()),
            rc => Err(map_response_code(rc)),
        });
        done
    }

    /// `END_DATA_TRANSFER`: close the transaction and return the device's response code.
    ///
    /// # Safety
    /// COM thread only.
    pub(crate) unsafe fn end(&mut self, t: &Transfer) -> Result<ResponseCode, Error> {
        let p = command(&WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER)?;
        p.SetStringValue(
            &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT,
            PCWSTR(t.context.as_ptr()),
        )
        .map_err(map_hresult)?;
        let r = self.send(&p)?;
        let rc = r
            .GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_RESPONSE_CODE)
            .map_err(map_hresult)?;
        Ok(ResponseCode::from(rc as u16))
    }

    /// A whole small data-to-read transaction (a dataset), buffered.
    unsafe fn read_whole(
        &mut self,
        opcode: u16,
        params: &[u32],
    ) -> Result<(ResponseCode, Vec<u8>), Error> {
        let t = self.begin(opcode, params)?;
        let mut data = Vec::new();
        let mut buf = Vec::new();
        let mut got = 0u64;
        let outcome = loop {
            if t.total() == Some(got) {
                break Ok(());
            }
            let ask = next_ask(&t, got);
            match self.read(&t, ask, &mut buf) {
                Ok(c) => {
                    got += c.len() as u64;
                    data.extend_from_slice(&c);
                    if data_done(&t, got, c.len(), ask) {
                        break Ok(());
                    }
                }
                Err(e) => break Err(e),
            }
        };
        let rc = self.end(&t);
        outcome?;
        Ok((rc?, data))
    }

    /// `SendCommand`, turning a failed `WPD_PROPERTY_COMMON_HRESULT` into an error, and retrying
    /// once with `WPD_API_OPTION_IOCTL_ACCESS` on `E_ACCESSDENIED` (the `SendCommand` docs' advice;
    /// the Pixel never needed it).
    unsafe fn send(&mut self, p: &IPortableDeviceValues) -> Result<IPortableDeviceValues, Error> {
        if self.ioctl_access {
            p.SetUnsignedIntegerValue(&WPD_API_OPTION_IOCTL_ACCESS, IOCTL_READ_WRITE)
                .map_err(map_hresult)?;
        }
        match self.send_once(p) {
            Err(e) if e.code().0 == E_ACCESSDENIED && !self.ioctl_access => {
                p.SetUnsignedIntegerValue(&WPD_API_OPTION_IOCTL_ACCESS, IOCTL_READ_WRITE)
                    .map_err(map_hresult)?;
                let r = self.send_once(p).map_err(map_hresult)?;
                diag_debug!("wpd: passthrough needed WPD_API_OPTION_IOCTL_ACCESS");
                self.ioctl_access = true;
                Ok(r)
            }
            other => other.map_err(map_hresult),
        }
    }

    unsafe fn send_once(
        &self,
        p: &IPortableDeviceValues,
    ) -> Result<IPortableDeviceValues, WinError> {
        let r = self.device.SendCommand(0, p)?;
        let hr: HRESULT = r.GetErrorValue(&WPD_PROPERTY_COMMON_HRESULT)?;
        hr.ok()?;
        Ok(r)
    }
}

/// A command parameter bag with the command's category and id filled in.
unsafe fn command(cmd: &PROPERTYKEY) -> Result<IPortableDeviceValues, Error> {
    let p: IPortableDeviceValues =
        CoCreateInstance(&PortableDeviceValues, None, CLSCTX_ALL).map_err(map_hresult)?;
    p.SetGuidValue(&WPD_PROPERTY_COMMON_COMMAND_CATEGORY, &cmd.fmtid)
        .map_err(map_hresult)?;
    p.SetUnsignedIntegerValue(&WPD_PROPERTY_COMMON_COMMAND_ID, cmd.pid)
        .map_err(map_hresult)?;
    Ok(p)
}

/// The MTP operation parameters, as a collection of `VT_UI4`.
unsafe fn u32_collection(values: &[u32]) -> Result<IPortableDevicePropVariantCollection, Error> {
    let coll: IPortableDevicePropVariantCollection =
        CoCreateInstance(&PortableDevicePropVariantCollection, None, CLSCTX_ALL)
            .map_err(map_hresult)?;
    for &v in values {
        let mut pv = PROPVARIANT::default();
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
        let added = coll.Add(&pv);
        // A VT_UI4 owns nothing; cleared for symmetry with the other collections.
        let _ = PropVariantClear(&mut pv);
        added.map_err(map_hresult)?;
    }
    Ok(coll)
}

/// Copy a COM-allocated buffer out of a result bag and free it.
unsafe fn take_buffer(r: &IPortableDeviceValues, key: &PROPERTYKEY) -> Result<Vec<u8>, Error> {
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len: u32 = 0;
    r.GetBufferValue(key, &mut ptr, &mut len)
        .map_err(map_hresult)?;
    if ptr.is_null() {
        return Ok(Vec::new());
    }
    let out = std::slice::from_raw_parts(ptr, len as usize).to_vec();
    CoTaskMemFree(Some(ptr as *const c_void));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_parses_from_o_hex_ids_only() {
        assert_eq!(mtp_handle_from_wpd_id("oEF"), Some(0xEF));
        assert_eq!(mtp_handle_from_wpd_id("o2c"), Some(0x2C));
        assert_eq!(mtp_handle_from_wpd_id("oFFFFFFFF"), Some(u32::MAX));
        assert_eq!(mtp_handle_from_wpd_id("o"), None);
        assert_eq!(mtp_handle_from_wpd_id("o100000000"), None);
        assert_eq!(mtp_handle_from_wpd_id("s10001"), None);
        assert_eq!(mtp_handle_from_wpd_id("oXYZ"), None);
        assert_eq!(mtp_handle_from_wpd_id("o+1"), None);
        assert_eq!(mtp_handle_from_wpd_id("DEVICE"), None);
    }

    #[test]
    fn ops_prefer_the_wide_op_and_limit_the_narrow_one_to_32_bits() {
        let both = PartialOps {
            wide: true,
            narrow: true,
        };
        assert_eq!(both.for_offset(5 << 32), Some(PartialOp::Wide));
        let narrow = PartialOps {
            wide: false,
            narrow: true,
        };
        assert_eq!(
            narrow.for_offset(u64::from(u32::MAX)),
            Some(PartialOp::Narrow)
        );
        assert_eq!(narrow.for_offset(u64::from(u32::MAX) + 1), None);
        assert_eq!(PartialOps::default().for_offset(0), None);
    }

    #[test]
    fn params_split_the_64_bit_offset() {
        assert_eq!(
            PartialOp::Wide.params(0xEF, 0x1_2345_6789, 0x40_0000),
            vec![0xEF, 0x2345_6789, 0x1, 0x40_0000]
        );
        assert_eq!(
            PartialOp::Narrow.params(0xEF, 0x1000, 7),
            vec![0xEF, 0x1000, 7]
        );
    }

    #[test]
    fn same_object_needs_name_and_size() {
        assert!(same_object("a.bin", 10, "a.bin", 10));
        assert!(!same_object("a.bin", 10, "b.bin", 10));
        assert!(!same_object("a.bin", 10, "a.bin", 11));
        // A 32-bit dataset saturates at 4 GiB.
        assert!(same_object("v.mp4", u64::from(u32::MAX), "v.mp4", 5 << 30));
        assert!(!same_object("v.mp4", u64::from(u32::MAX), "v.mp4", 10));
    }
}
