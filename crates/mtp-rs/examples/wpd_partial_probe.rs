//! Hardware probe for real partial reads on Windows WPD (#61): can we send MTP `GetPartialObject64`
//! (0x95C1) and `GetPartialObject` (0x101B) through WPD's MTP passthrough, and how do they compare
//! with reopening the forward-only resource stream?
//!
//! Self-contained: it drives the `windows` crate directly and uses nothing inside mtp-rs, so it
//! tests the Windows MTP driver, not our backend. The plan behind it, and what each result means,
//! is in `docs/specs/wpd-partial-reads.md` § Test plan.
//!
//! Read-only on the device by default. `--early-end` and `--interleave` poke at transaction edge
//! cases and can, in the worst case, leave the phone needing a replug; run them last.
//!
//! Windows-only; phone in MTP/File-transfer mode, unlocked, nothing else (Explorer, Photos) using it.
//!
//! Run: `cargo run --release -p mtp-rs --example wpd_partial_probe -- "<path>" [options]`
//!
//! `<path>` is the file's path on the device, like `DCIM/Camera/VID_20261101_120000.mp4`. A first
//! component that names a storage ("Internal shared storage/DCIM/...") picks that storage;
//! otherwise the first storage is used. Use a big file (1 GB or more) so the prefix costs show.
//!
//! Options:
//! - `--window BYTES`: bytes per compared read at each offset (default 4M; K/M/G suffixes work)
//! - `--offsets A,B,C`: byte offsets to compare (default: 0, the middle, and one window before the end)
//! - `--chunk BYTES`: bytes per `READ_DATA` call (default: the driver's optimal size)
//! - `--bench BYTES`: sequential throughput run from offset 0 (default 64M; 0 skips it)
//! - `--bench-window BYTES`: partial-read window size in the throughput run (default 8M)
//! - `--handle 0xNN`: MTP handle to use when the WPD object id doesn't look like `o<hex>`
//! - `--device N`: which WPD device to open (default 0, the first)
//! - `--early-end`: end a transfer after one chunk, then check the session still works
//! - `--interleave`: run other WPD calls in the middle of a passthrough transfer and with a
//!   resource stream open

#[cfg(windows)]
fn main() {
    if let Err(e) = probe::run() {
        eprintln!("\nprobe failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("wpd_partial_probe is Windows-only.");
}

#[cfg(windows)]
mod probe {
    use std::ffi::c_void;
    use std::time::{Duration, Instant};

    use windows::core::{Error as WinError, HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Devices::PortableDevices::*;
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, IStream, CLSCTX_ALL, COINIT_MULTITHREADED,
        STGM_READ, STREAM_SEEK_SET,
    };
    use windows::Win32::System::Variant::VT_UI4;

    type R<T> = Result<T, Box<dyn std::error::Error>>;

    const OP_GET_OBJECT_INFO: u32 = 0x1008;
    const OP_GET_PARTIAL_OBJECT: u32 = 0x101B;
    const OP_GET_PARTIAL_OBJECT_64: u32 = 0x95C1;
    const RC_OK: u32 = 0x2001;
    /// `FILE_READ_ACCESS | FILE_WRITE_ACCESS` from `winioctl.h`, for `WPD_API_OPTION_IOCTL_ACCESS`.
    const IOCTL_READ_WRITE: u32 = 0x1 | 0x2;
    const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
    const DEFAULT_CHUNK: u32 = 256 * 1024;
    const MIB: f64 = 1024.0 * 1024.0;

    struct Opts {
        path: String,
        window: u64,
        offsets: Option<Vec<u64>>,
        chunk: Option<u32>,
        bench: u64,
        bench_window: u64,
        handle: Option<u32>,
        device: usize,
        early_end: bool,
        interleave: bool,
    }

    fn parse_size(s: &str) -> R<u64> {
        let s = s.trim();
        let (num, mult) = match s.chars().last() {
            Some('K' | 'k') => (&s[..s.len() - 1], 1024),
            Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
            Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
            _ => (s, 1),
        };
        let n: u64 = match num.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16)?,
            None => num.parse()?,
        };
        Ok(n * mult)
    }

    fn parse_opts() -> R<Opts> {
        let mut o = Opts {
            path: String::new(),
            window: 4 * 1024 * 1024,
            offsets: None,
            chunk: None,
            bench: 64 * 1024 * 1024,
            bench_window: 8 * 1024 * 1024,
            handle: None,
            device: 0,
            early_end: false,
            interleave: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            let mut val = || args.next().ok_or_else(|| format!("{a} needs a value"));
            match a.as_str() {
                "--window" => o.window = parse_size(&val()?)?.max(1),
                "--offsets" => {
                    o.offsets = Some(val()?.split(',').map(parse_size).collect::<R<Vec<u64>>>()?)
                }
                "--chunk" => o.chunk = Some(parse_size(&val()?)?.clamp(1, u32::MAX as u64) as u32),
                "--bench" => o.bench = parse_size(&val()?)?,
                "--bench-window" => o.bench_window = parse_size(&val()?)?.max(1),
                "--handle" => o.handle = Some(parse_size(&val()?)? as u32),
                "--device" => o.device = val()?.parse()?,
                "--early-end" => o.early_end = true,
                "--interleave" => o.interleave = true,
                s if s.starts_with("--") => return Err(format!("unknown option {s}").into()),
                s => o.path = s.trim_matches('/').to_string(),
            }
        }
        if o.path.is_empty() {
            return Err(
                "usage: wpd_partial_probe <path-on-device> [options] (see the file header)".into(),
            );
        }
        Ok(o)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn take_pwstr(p: PWSTR) -> String {
        if p.is_null() {
            return String::new();
        }
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }

    fn hr_text(e: &WinError) -> String {
        format!("HRESULT {:#010x} ({})", e.code().0 as u32, e.message())
    }

    fn mbps(bytes: u64, d: Duration) -> f64 {
        bytes as f64 / MIB / d.as_secs_f64().max(1e-9)
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    /// Where two byte slices first differ, or `None` when they're equal.
    fn first_mismatch(a: &[u8], b: &[u8]) -> Option<usize> {
        if a == b {
            return None;
        }
        Some(
            a.iter()
                .zip(b)
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len())),
        )
    }

    unsafe fn new_values() -> R<IPortableDeviceValues> {
        Ok(CoCreateInstance(&PortableDeviceValues, None, CLSCTX_ALL)?)
    }

    /// A command parameter bag with the command's category and id filled in.
    unsafe fn command(cmd: &PROPERTYKEY) -> R<IPortableDeviceValues> {
        let p = new_values()?;
        p.SetGuidValue(&WPD_PROPERTY_COMMON_COMMAND_CATEGORY, &cmd.fmtid)?;
        p.SetUnsignedIntegerValue(&WPD_PROPERTY_COMMON_COMMAND_ID, cmd.pid)?;
        Ok(p)
    }

    unsafe fn u32_collection(values: &[u32]) -> R<IPortableDevicePropVariantCollection> {
        let coll: IPortableDevicePropVariantCollection =
            CoCreateInstance(&PortableDevicePropVariantCollection, None, CLSCTX_ALL)?;
        for &v in values {
            let mut pv = PROPVARIANT::default();
            let inner = &mut *pv.Anonymous.Anonymous;
            inner.vt = VT_UI4;
            inner.Anonymous.ulVal = v;
            coll.Add(&pv)?;
        }
        Ok(coll)
    }

    unsafe fn read_u32_collection(coll: &IPortableDevicePropVariantCollection) -> Vec<u32> {
        let count = 0u32;
        if coll.GetCount(&count).is_err() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut pv = PROPVARIANT::default();
            if coll.GetAt(i, &pv).is_ok() {
                let inner = &pv.Anonymous.Anonymous;
                if inner.vt == VT_UI4 {
                    out.push(inner.Anonymous.ulVal);
                }
            }
            let _ = PropVariantClear(&mut pv);
        }
        out
    }

    unsafe fn key_collection(keys: &[PROPERTYKEY]) -> R<IPortableDeviceKeyCollection> {
        let coll: IPortableDeviceKeyCollection =
            CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_ALL)?;
        for k in keys {
            coll.Add(k)?;
        }
        Ok(coll)
    }

    /// Copy a COM-allocated buffer out of a result bag and free it.
    unsafe fn take_buffer(r: &IPortableDeviceValues, key: &PROPERTYKEY) -> R<Vec<u8>> {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len: u32 = 0;
        r.GetBufferValue(key, &mut ptr, &mut len)?;
        if ptr.is_null() {
            return Ok(Vec::new());
        }
        let out = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        CoTaskMemFree(Some(ptr as *const c_void));
        Ok(out)
    }

    /// One passthrough data-to-read transfer that `EXECUTE_COMMAND_WITH_DATA_TO_READ` started.
    struct Transfer {
        context: Vec<u16>,
        total: u64,
        optimal: u32,
    }

    struct PassRead {
        response: u32,
        params: Vec<u32>,
        data: Vec<u8>,
        total: u64,
        optimal: u32,
        reads: u32,
        t_execute: Duration,
        t_data: Duration,
        t_end: Duration,
    }

    impl PassRead {
        fn total_time(&self) -> Duration {
            self.t_execute + self.t_data + self.t_end
        }
    }

    struct Probe {
        device: IPortableDevice,
        content: IPortableDeviceContent,
        props: IPortableDeviceProperties,
        resources: IPortableDeviceResources,
        /// Set once a plain `SendCommand` came back `E_ACCESSDENIED` and the retry with an explicit
        /// IOCTL access level worked, so later commands send it straight away.
        ioctl_access: bool,
    }

    impl Probe {
        unsafe fn open(index: usize) -> R<Self> {
            let manager: IPortableDeviceManager =
                CoCreateInstance(&PortableDeviceManager, None, CLSCTX_ALL)?;
            let mut count = 0u32;
            manager.GetDevices(std::ptr::null_mut(), &mut count)?;
            let mut ids = vec![PWSTR::null(); count as usize];
            if count > 0 {
                manager.GetDevices(ids.as_mut_ptr(), &mut count)?;
            }
            let ids: Vec<String> = ids.into_iter().map(|p| take_pwstr(p)).collect();
            println!("WPD devices: {}", ids.len());
            for (i, id) in ids.iter().enumerate() {
                println!("  [{i}] {id}");
            }
            let pnp = ids.get(index).ok_or("no WPD device at that index")?;

            let device: IPortableDevice = CoCreateInstance(&PortableDevice, None, CLSCTX_ALL)?;
            let client = new_values()?;
            let name = wide("mtp-rs wpd_partial_probe");
            client.SetStringValue(&WPD_CLIENT_NAME, PCWSTR(name.as_ptr()))?;
            let pnp_w = wide(pnp);
            device.Open(PCWSTR(pnp_w.as_ptr()), &client)?;
            let content = device.Content()?;
            let props = content.Properties()?;
            let resources = content.Transfer()?;
            Ok(Self {
                device,
                content,
                props,
                resources,
                ioctl_access: false,
            })
        }

        /// `SendCommand`, turning a failed `WPD_PROPERTY_COMMON_HRESULT` into an error.
        unsafe fn send(
            &self,
            p: &IPortableDeviceValues,
        ) -> Result<IPortableDeviceValues, WinError> {
            let r = self.device.SendCommand(0, p)?;
            let hr: HRESULT = r.GetErrorValue(&WPD_PROPERTY_COMMON_HRESULT)?;
            hr.ok()?;
            Ok(r)
        }

        /// `send`, retrying once with `WPD_API_OPTION_IOCTL_ACCESS` on `E_ACCESSDENIED`.
        unsafe fn send_mtp(
            &mut self,
            p: &IPortableDeviceValues,
        ) -> Result<IPortableDeviceValues, WinError> {
            if self.ioctl_access {
                p.SetUnsignedIntegerValue(&WPD_API_OPTION_IOCTL_ACCESS, IOCTL_READ_WRITE)?;
            }
            match self.send(p) {
                Err(e) if e.code().0 == E_ACCESSDENIED && !self.ioctl_access => {
                    println!("    (E_ACCESSDENIED; retrying with WPD_API_OPTION_IOCTL_ACCESS = read|write)");
                    p.SetUnsignedIntegerValue(&WPD_API_OPTION_IOCTL_ACCESS, IOCTL_READ_WRITE)?;
                    let r = self.send(p)?;
                    self.ioctl_access = true;
                    Ok(r)
                }
                other => other,
            }
        }

        // ---- capabilities -----------------------------------------------------------------------

        unsafe fn report_capabilities(&mut self) -> R<()> {
            println!("\n== Driver capabilities ==");
            let caps = self.device.Capabilities()?;
            let cmds = caps.GetSupportedCommands()?;
            let count = 0u32;
            cmds.GetCount(&count)?;
            let mut supported = Vec::new();
            for i in 0..count {
                let k = PROPERTYKEY::default();
                if cmds.GetAt(i, &k).is_ok() {
                    supported.push(k);
                }
            }
            let has = |k: &PROPERTYKEY| {
                supported
                    .iter()
                    .any(|c| c.fmtid == k.fmtid && c.pid == k.pid)
            };
            for (name, key) in [
                (
                    "MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ",
                    WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ,
                ),
                ("MTP_EXT_READ_DATA", WPD_COMMAND_MTP_EXT_READ_DATA),
                (
                    "MTP_EXT_END_DATA_TRANSFER",
                    WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER,
                ),
                (
                    "MTP_EXT_GET_SUPPORTED_VENDOR_OPCODES",
                    WPD_COMMAND_MTP_EXT_GET_SUPPORTED_VENDOR_OPCODES,
                ),
                ("OBJECT_RESOURCES_OPEN", WPD_COMMAND_OBJECT_RESOURCES_OPEN),
                ("OBJECT_RESOURCES_READ", WPD_COMMAND_OBJECT_RESOURCES_READ),
                ("OBJECT_RESOURCES_SEEK", WPD_COMMAND_OBJECT_RESOURCES_SEEK),
                (
                    "OBJECT_RESOURCES_SEEK_IN_UNITS",
                    WPD_COMMAND_OBJECT_RESOURCES_SEEK_IN_UNITS,
                ),
            ] {
                println!(
                    "  {name}: {}",
                    if has(&key) { "listed" } else { "NOT listed" }
                );
            }

            for (name, cmd) in [
                ("OBJECT_RESOURCES_SEEK", WPD_COMMAND_OBJECT_RESOURCES_SEEK),
                ("OBJECT_RESOURCES_OPEN", WPD_COMMAND_OBJECT_RESOURCES_OPEN),
                ("OBJECT_RESOURCES_READ", WPD_COMMAND_OBJECT_RESOURCES_READ),
            ] {
                match caps.GetCommandOptions(&cmd) {
                    Ok(opts) => {
                        let b = |k: &PROPERTYKEY| match opts.GetBoolValue(k) {
                            Ok(v) => format!("{}", v.as_bool()),
                            Err(_) => "absent".to_string(),
                        };
                        println!(
                            "  options of {name}: SEEK_ON_READ_SUPPORTED={}, NO_INPUT_BUFFER_ON_READ={}",
                            b(&WPD_OPTION_OBJECT_RESOURCES_SEEK_ON_READ_SUPPORTED),
                            b(&WPD_OPTION_OBJECT_RESOURCES_NO_INPUT_BUFFER_ON_READ)
                        );
                    }
                    Err(e) => println!("  options of {name}: {}", hr_text(&e)),
                }
            }

            let p = command(&WPD_COMMAND_MTP_EXT_GET_SUPPORTED_VENDOR_OPCODES)?;
            match self.send_mtp(&p) {
                Ok(r) => {
                    let codes = r
                        .GetIPortableDevicePropVariantCollectionValue(
                            &WPD_PROPERTY_MTP_EXT_VENDOR_OPERATION_CODES,
                        )
                        .map(|c| read_u32_collection(&c))
                        .unwrap_or_default();
                    let list: Vec<String> = codes.iter().map(|c| format!("{c:#06x}")).collect();
                    println!("  vendor opcodes the driver reports: [{}]", list.join(", "));
                    println!(
                        "  0x95C1 GetPartialObject64 among them: {}",
                        codes.contains(&OP_GET_PARTIAL_OBJECT_64)
                    );
                }
                Err(e) => println!("  GET_SUPPORTED_VENDOR_OPCODES: {}", hr_text(&e)),
            }
            Ok(())
        }

        // ---- finding the file -------------------------------------------------------------------

        unsafe fn children(&self, parent: &str) -> R<Vec<String>> {
            let w = wide(parent);
            let e = self.content.EnumObjects(0, PCWSTR(w.as_ptr()), None)?;
            let mut out = Vec::new();
            loop {
                let mut batch = [PWSTR::null(); 64];
                let mut fetched = 0u32;
                let hr = e.Next(&mut batch, &mut fetched);
                for p in batch.iter().take(fetched as usize) {
                    out.push(take_pwstr(*p));
                }
                if fetched == 0 || hr.0 != 0 {
                    break;
                }
            }
            Ok(out)
        }

        unsafe fn name_of(&self, id: &str) -> String {
            let w = wide(id);
            let Ok(keys) = key_collection(&[WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_NAME]) else {
                return String::new();
            };
            let Ok(v) = self.props.GetValues(PCWSTR(w.as_ptr()), &keys) else {
                return String::new();
            };
            let get = |k: &PROPERTYKEY| {
                v.GetStringValue(k)
                    .map(|p| take_pwstr(p))
                    .ok()
                    .filter(|s| !s.is_empty())
            };
            get(&WPD_OBJECT_ORIGINAL_FILE_NAME)
                .or_else(|| get(&WPD_OBJECT_NAME))
                .unwrap_or_default()
        }

        /// Resolve `path` to (parent WPD id, file WPD id).
        unsafe fn resolve(&self, path: &str) -> R<(String, String)> {
            let storages = self.children("DEVICE")?;
            if storages.is_empty() {
                return Err(
                    "device has no storages (is it unlocked and in file-transfer mode?)".into(),
                );
            }
            let mut parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            let mut current = storages[0].clone();
            if let Some(first) = parts.first() {
                for s in &storages {
                    if self.name_of(s).eq_ignore_ascii_case(first) {
                        current = s.clone();
                        parts.remove(0);
                        break;
                    }
                }
            }
            println!("  storage: {current} ({})", self.name_of(&current));
            let mut parent = current.clone();
            for part in parts {
                let kids = self.children(&current)?;
                let next = kids
                    .into_iter()
                    .find(|k| self.name_of(k).eq_ignore_ascii_case(part))
                    .ok_or_else(|| format!("'{part}' not found under {current}"))?;
                parent = current;
                current = next;
            }
            Ok((parent, current))
        }

        // ---- MTP passthrough ---------------------------------------------------------------------

        unsafe fn begin_read(&mut self, opcode: u32, params: &[u32]) -> R<Transfer> {
            let p = command(&WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ)?;
            p.SetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_OPERATION_CODE, opcode)?;
            let coll = u32_collection(params)?;
            p.SetIPortableDevicePropVariantCollectionValue(
                &WPD_PROPERTY_MTP_EXT_OPERATION_PARAMS,
                &coll,
            )?;
            let r = self.send_mtp(&p)?;
            let context = take_pwstr(r.GetStringValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT)?);
            // The reference page says VT_UI8; Microsoft's own sample reads it as a ULONG.
            let total = match r
                .GetUnsignedLargeIntegerValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE)
            {
                Ok(v) => v,
                Err(_) => r
                    .GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE)
                    .map(u64::from)?,
            };
            let optimal = r
                .GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_OPTIMAL_TRANSFER_BUFFER_SIZE)
                .unwrap_or(0);
            Ok(Transfer {
                context: wide(&context),
                total,
                optimal,
            })
        }

        unsafe fn read_chunk(&mut self, t: &Transfer, n: u32) -> R<Vec<u8>> {
            let p = command(&WPD_COMMAND_MTP_EXT_READ_DATA)?;
            p.SetStringValue(
                &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT,
                PCWSTR(t.context.as_ptr()),
            )?;
            // The driver insists on an input buffer of the read size, then returns the data in a
            // separate buffer in the results (Microsoft's sample notes the same).
            let buf = vec![0u8; n as usize];
            p.SetBufferValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_DATA, &buf)?;
            p.SetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_READ, n)?;
            let r = self.send_mtp(&p)?;
            take_buffer(&r, &WPD_PROPERTY_MTP_EXT_TRANSFER_DATA)
        }

        unsafe fn end(&mut self, t: &Transfer) -> R<(u32, Vec<u32>)> {
            let p = command(&WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER)?;
            p.SetStringValue(
                &WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT,
                PCWSTR(t.context.as_ptr()),
            )?;
            let r = self.send_mtp(&p)?;
            let rc = r.GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_RESPONSE_CODE)?;
            let params = r
                .GetIPortableDevicePropVariantCollectionValue(&WPD_PROPERTY_MTP_EXT_RESPONSE_PARAMS)
                .map(|c| read_u32_collection(&c))
                .unwrap_or_default();
            Ok((rc, params))
        }

        /// A whole data-to-read transaction. `keep` false counts the bytes without storing them.
        unsafe fn mtp_read(
            &mut self,
            opcode: u32,
            params: &[u32],
            chunk: Option<u32>,
            keep: bool,
        ) -> R<PassRead> {
            let t0 = Instant::now();
            let t = self.begin_read(opcode, params)?;
            let t_execute = t0.elapsed();
            let chunk = chunk
                .or((t.optimal > 0).then_some(t.optimal))
                .unwrap_or(DEFAULT_CHUNK) as u64;
            let unknown = t.total == u32::MAX as u64;
            let mut data = Vec::new();
            let mut got = 0u64;
            let mut reads = 0u32;
            let t1 = Instant::now();
            while unknown || got < t.total {
                let n = if unknown {
                    chunk
                } else {
                    chunk.min(t.total - got)
                };
                let c = self.read_chunk(&t, n as u32)?;
                reads += 1;
                got += c.len() as u64;
                if keep {
                    data.extend_from_slice(&c);
                }
                if c.is_empty() || (unknown && (c.len() as u64) < n) {
                    break;
                }
            }
            let t_data = t1.elapsed();
            let t2 = Instant::now();
            let (response, rparams) = self.end(&t)?;
            let t_end = t2.elapsed();
            if !keep {
                data = Vec::new();
            }
            Ok(PassRead {
                response,
                params: rparams,
                data,
                total: if unknown { got } else { t.total },
                optimal: t.optimal,
                reads,
                t_execute,
                t_data,
                t_end,
            })
        }

        unsafe fn partial(
            &mut self,
            opcode: u32,
            handle: u32,
            offset: u64,
            len: u32,
            chunk: Option<u32>,
            keep: bool,
        ) -> R<PassRead> {
            let params: Vec<u32> = if opcode == OP_GET_PARTIAL_OBJECT_64 {
                vec![handle, offset as u32, (offset >> 32) as u32, len]
            } else {
                vec![handle, offset as u32, len]
            };
            self.mtp_read(opcode, &params, chunk, keep)
        }

        // ---- resource stream ---------------------------------------------------------------------

        unsafe fn open_stream(&self, id: &str) -> R<(IStream, u32)> {
            let w = wide(id);
            let mut optimal = 0u32;
            let mut stream: Option<IStream> = None;
            self.resources.GetStream(
                PCWSTR(w.as_ptr()),
                &WPD_RESOURCE_DEFAULT,
                STGM_READ.0,
                &mut optimal,
                &mut stream,
            )?;
            Ok((stream.ok_or("GetStream returned no stream")?, optimal))
        }

        // ---- raw resource commands (what IStream wraps) -------------------------------------------

        unsafe fn raw_seek_probe(
            &mut self,
            id: &str,
            offset: u64,
            len: u32,
            reference: Option<&[u8]>,
        ) {
            println!(
                "\n== Raw WPD_COMMAND_OBJECT_RESOURCES_* through SendCommand (offset {offset}) =="
            );
            let res: R<()> = (|| {
                let p = command(&WPD_COMMAND_OBJECT_RESOURCES_OPEN)?;
                let idw = wide(id);
                p.SetStringValue(
                    &WPD_PROPERTY_OBJECT_RESOURCES_OBJECT_ID,
                    PCWSTR(idw.as_ptr()),
                )?;
                let keys = key_collection(&[WPD_RESOURCE_DEFAULT])?;
                p.SetIPortableDeviceKeyCollectionValue(
                    &WPD_PROPERTY_OBJECT_RESOURCES_RESOURCE_KEYS,
                    &keys,
                )?;
                p.SetUnsignedIntegerValue(&WPD_PROPERTY_OBJECT_RESOURCES_ACCESS_MODE, STGM_READ.0)?;
                let t0 = Instant::now();
                let r = self.send(&p)?;
                let ctx = take_pwstr(r.GetStringValue(&WPD_PROPERTY_OBJECT_RESOURCES_CONTEXT)?);
                let ctxw = wide(&ctx);
                println!("  OPEN: ok in {:.1} ms", ms(t0.elapsed()));

                let p = command(&WPD_COMMAND_OBJECT_RESOURCES_SEEK)?;
                p.SetStringValue(
                    &WPD_PROPERTY_OBJECT_RESOURCES_CONTEXT,
                    PCWSTR(ctxw.as_ptr()),
                )?;
                p.SetSignedLargeIntegerValue(
                    &WPD_PROPERTY_OBJECT_RESOURCES_SEEK_OFFSET,
                    offset as i64,
                )?;
                p.SetUnsignedIntegerValue(
                    &WPD_PROPERTY_OBJECT_RESOURCES_SEEK_ORIGIN_FLAG,
                    STREAM_SEEK_SET.0,
                )?;
                let t0 = Instant::now();
                let seeked = match self.send(&p) {
                    Ok(r) => {
                        let pos = r.GetUnsignedLargeIntegerValue(
                            &WPD_PROPERTY_OBJECT_RESOURCES_POSITION_FROM_START,
                        );
                        println!(
                            "  SEEK: ok in {:.1} ms, position {:?}",
                            ms(t0.elapsed()),
                            pos.ok()
                        );
                        true
                    }
                    Err(e) => {
                        println!("  SEEK: {} after {:.1} ms", hr_text(&e), ms(t0.elapsed()));
                        false
                    }
                };

                if seeked {
                    let p = command(&WPD_COMMAND_OBJECT_RESOURCES_READ)?;
                    p.SetStringValue(
                        &WPD_PROPERTY_OBJECT_RESOURCES_CONTEXT,
                        PCWSTR(ctxw.as_ptr()),
                    )?;
                    p.SetUnsignedIntegerValue(
                        &WPD_PROPERTY_OBJECT_RESOURCES_NUM_BYTES_TO_READ,
                        len,
                    )?;
                    let buf = vec![0u8; len as usize];
                    p.SetBufferValue(&WPD_PROPERTY_OBJECT_RESOURCES_DATA, &buf)?;
                    let t0 = Instant::now();
                    match self.send(&p) {
                        Ok(r) => {
                            let data = take_buffer(&r, &WPD_PROPERTY_OBJECT_RESOURCES_DATA)?;
                            let same = reference
                                .map(|rf| first_mismatch(rf, &data[..data.len().min(rf.len())]));
                            println!(
                                "  READ after SEEK: {} bytes in {:.1} ms; matches stream bytes: {}",
                                data.len(),
                                ms(t0.elapsed()),
                                match same {
                                    Some(None) => "yes".to_string(),
                                    Some(Some(i)) => format!("NO, first mismatch at {i}"),
                                    None => "no reference".to_string(),
                                }
                            );
                        }
                        Err(e) => println!("  READ after SEEK: {}", hr_text(&e)),
                    }
                }

                let p = command(&WPD_COMMAND_OBJECT_RESOURCES_CLOSE)?;
                p.SetStringValue(
                    &WPD_PROPERTY_OBJECT_RESOURCES_CONTEXT,
                    PCWSTR(ctxw.as_ptr()),
                )?;
                match self.send(&p) {
                    Ok(_) => println!("  CLOSE: ok"),
                    Err(e) => println!("  CLOSE: {}", hr_text(&e)),
                }
                Ok(())
            })();
            if let Err(e) = res {
                println!("  failed: {e}");
            }
        }
    }

    unsafe fn read_up_to(
        stream: &IStream,
        n: u64,
        buf: &mut [u8],
        keep: bool,
    ) -> R<(Vec<u8>, u64)> {
        let mut out = Vec::new();
        let mut got = 0u64;
        while got < n {
            let want = (n - got).min(buf.len() as u64) as usize;
            let mut read = 0u32;
            stream
                .Read(
                    buf.as_mut_ptr() as *mut c_void,
                    want as u32,
                    Some(&mut read),
                )
                .ok()?;
            if read == 0 {
                break;
            }
            if keep {
                out.extend_from_slice(&buf[..read as usize]);
            }
            got += read as u64;
        }
        Ok((out, got))
    }

    struct Row {
        offset: u64,
        stream_ms: Option<f64>,
        pass64_ms: Option<f64>,
        pass32_ms: Option<f64>,
    }

    fn describe(label: &str, r: &R<PassRead>, reference: Option<&[u8]>) -> Option<f64> {
        match r {
            Ok(p) => {
                let matches = match reference {
                    Some(rf) => match first_mismatch(rf, &p.data) {
                        None => "bytes match the stream".to_string(),
                        Some(i) => format!(
                            "bytes DIFFER from the stream at {i} (got {}, want {})",
                            p.data.len(),
                            rf.len()
                        ),
                    },
                    None => "no stream reference".to_string(),
                };
                println!(
                    "  {label}: response {:#06x} params {:x?}; total {} B (optimal chunk {} B, {} reads); execute {:.1} ms, data {:.1} ms, end {:.1} ms; {:.1} MiB/s; {matches}",
                    p.response,
                    p.params,
                    p.total,
                    p.optimal,
                    p.reads,
                    ms(p.t_execute),
                    ms(p.t_data),
                    ms(p.t_end),
                    mbps(p.total, p.total_time()),
                );
                (p.response == RC_OK).then(|| ms(p.total_time()))
            }
            Err(e) => {
                println!("  {label}: failed: {e}");
                None
            }
        }
    }

    pub fn run() -> R<()> {
        let o = parse_opts()?;
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let mut p = Probe::open(o.device)?;
            p.report_capabilities()?;

            println!("\n== File ==");
            let (parent_id, id) = p.resolve(&o.path)?;
            let idw = wide(&id);
            let keys = key_collection(&[
                WPD_OBJECT_SIZE,
                WPD_OBJECT_PERSISTENT_UNIQUE_ID,
                WPD_OBJECT_ORIGINAL_FILE_NAME,
                WPD_OBJECT_PARENT_ID,
            ])?;
            let v = p.props.GetValues(PCWSTR(idw.as_ptr()), &keys)?;
            let size = v.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE)?;
            let name = v
                .GetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME)
                .map(|s| take_pwstr(s))
                .unwrap_or_default();
            let puoid = v
                .GetStringValue(&WPD_OBJECT_PERSISTENT_UNIQUE_ID)
                .map(|s| take_pwstr(s))
                .unwrap_or_default();
            println!("  WPD object id: {id}");
            println!("  parent id: {parent_id}");
            println!(
                "  name: {name}, size: {size} B ({:.1} MiB)",
                size as f64 / MIB
            );
            println!("  WPD_OBJECT_PERSISTENT_UNIQUE_ID: {puoid}");
            match p.resources.GetResourceAttributes(PCWSTR(idw.as_ptr()), &WPD_RESOURCE_DEFAULT) {
                Ok(a) => println!(
                    "  default resource: TOTAL_SIZE={:?}, CAN_READ={:?}, OPTIMAL_READ_BUFFER_SIZE={:?}",
                    a.GetUnsignedLargeIntegerValue(&WPD_RESOURCE_ATTRIBUTE_TOTAL_SIZE).ok(),
                    a.GetBoolValue(&WPD_RESOURCE_ATTRIBUTE_CAN_READ).ok().map(|b| b.as_bool()),
                    a.GetUnsignedIntegerValue(&WPD_RESOURCE_ATTRIBUTE_OPTIMAL_READ_BUFFER_SIZE).ok(),
                ),
                Err(e) => println!("  default resource attributes: {}", hr_text(&e)),
            }

            let parsed = id
                .strip_prefix('o')
                .and_then(|h| u32::from_str_radix(h, 16).ok());
            let handle = o.handle.or(parsed);
            println!(
                "  MTP handle: {} ({})",
                handle
                    .map(|h| format!("{h:#x}"))
                    .unwrap_or_else(|| "unknown".into()),
                if o.handle.is_some() {
                    "from --handle"
                } else if parsed.is_some() {
                    "parsed from o<hex>"
                } else {
                    "object id isn't o<hex>; pass --handle"
                }
            );

            if let Some(h) = handle {
                println!(
                    "\n== Handle mapping check: passthrough GetObjectInfo (0x1008) on {h:#x} =="
                );
                match p.mtp_read(OP_GET_OBJECT_INFO, &[h], None, true) {
                    Ok(r) => {
                        println!(
                            "  response {:#06x}, {} B dataset, execute {:.1} ms",
                            r.response,
                            r.data.len(),
                            ms(r.t_execute)
                        );
                        if r.response == RC_OK && r.data.len() > 53 {
                            let d = &r.data;
                            let u32_at =
                                |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
                            let csize = u32_at(8);
                            let parent = u32_at(38);
                            let n = d[52] as usize;
                            let units: Vec<u16> = (0..n.saturating_sub(1))
                                .filter_map(|k| {
                                    d.get(53 + 2 * k..55 + 2 * k)
                                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                                })
                                .collect();
                            let mtp_name = String::from_utf16_lossy(&units);
                            let size_ok = csize as u64 == size
                                || (csize == u32::MAX && size >= u32::MAX as u64);
                            println!("  MTP filename {mtp_name:?}, compressed size {csize}, parent {parent:#x}");
                            println!(
                                "  matches WPD: name {}, size {}{}",
                                mtp_name.eq_ignore_ascii_case(&name),
                                size_ok,
                                if mtp_name.eq_ignore_ascii_case(&name) && size_ok {
                                    "  => handle mapping CONFIRMED"
                                } else {
                                    "  => handle mapping WRONG"
                                }
                            );
                        }
                    }
                    Err(e) => println!("  failed: {e}"),
                }
            }

            // ---- Seek on the stream ----------------------------------------------------------------
            let mid = size / 2;
            println!("\n== IStream::Seek ==");
            {
                let (s, optimal) = p.open_stream(&id)?;
                println!("  GetStream optimal buffer size: {optimal} B");
                match s.Seek(mid as i64, STREAM_SEEK_SET, None) {
                    Ok(()) => println!("  Seek({mid}): ok (seekable stream)"),
                    Err(e) => println!("  Seek({mid}): {}", hr_text(&e)),
                }
            }

            // ---- per-offset comparison -------------------------------------------------------------
            let offsets = o.offsets.clone().unwrap_or_else(|| {
                let mut v = vec![0, mid, size.saturating_sub(o.window)];
                v.dedup();
                v
            });
            let mut rows = Vec::new();
            let mut references: Vec<(u64, Vec<u8>)> = Vec::new();
            for &off in &offsets {
                if off >= size {
                    println!("\n== Offset {off}: past the end ({size}), skipped ==");
                    continue;
                }
                let len = o.window.min(size - off).min(u32::MAX as u64) as u32;
                println!(
                    "\n== Offset {off} ({:.1}% in), {len} B ==",
                    off as f64 * 100.0 / size.max(1) as f64
                );

                // Stream: reopen, read-and-discard the prefix, read the window.
                let t0 = Instant::now();
                let stream_res: R<(Vec<u8>, Duration, Duration, Duration)> = (|| {
                    let (s, optimal) = p.open_stream(&id)?;
                    let t_open = t0.elapsed();
                    let mut buf = vec![0u8; (optimal.max(DEFAULT_CHUNK)) as usize];
                    let t1 = Instant::now();
                    let (_, skipped) = read_up_to(&s, off, &mut buf, false)?;
                    if skipped < off {
                        return Err(format!("stream ended after {skipped} B of the prefix").into());
                    }
                    let t_skip = t1.elapsed();
                    let t2 = Instant::now();
                    let (data, _) = read_up_to(&s, len as u64, &mut buf, true)?;
                    Ok((data, t_open, t_skip, t2.elapsed()))
                })();
                let stream_ms = match &stream_res {
                    Ok((data, t_open, t_skip, t_read)) => {
                        let total = *t_open + *t_skip + *t_read;
                        println!(
                            "  stream reopen + discard: open {:.1} ms, discard {off} B in {:.1} ms ({:.1} MiB/s), read {} B in {:.1} ms; total {:.1} ms",
                            ms(*t_open),
                            ms(*t_skip),
                            mbps(off, *t_skip),
                            data.len(),
                            ms(*t_read),
                            ms(total)
                        );
                        Some(ms(total))
                    }
                    Err(e) => {
                        println!("  stream reopen + discard: failed: {e}");
                        None
                    }
                };
                let reference = stream_res.ok().map(|(d, ..)| d);

                let (pass64_ms, pass32_ms) = match handle {
                    Some(h) => {
                        let r64 = p.partial(OP_GET_PARTIAL_OBJECT_64, h, off, len, o.chunk, true);
                        let a = describe(
                            "passthrough GetPartialObject64 (0x95C1)",
                            &r64,
                            reference.as_deref(),
                        );
                        let b = if off <= u32::MAX as u64 {
                            let r32 = p.partial(OP_GET_PARTIAL_OBJECT, h, off, len, o.chunk, true);
                            describe(
                                "passthrough GetPartialObject (0x101B)",
                                &r32,
                                reference.as_deref(),
                            )
                        } else {
                            println!("  passthrough GetPartialObject (0x101B): skipped, offset past 4 GiB");
                            None
                        };
                        (a, b)
                    }
                    None => (None, None),
                };
                if let Some(r) = reference {
                    references.push((off, r));
                }
                rows.push(Row {
                    offset: off,
                    stream_ms,
                    pass64_ms,
                    pass32_ms,
                });
            }

            // ---- raw resource commands: does SendCommand SEEK work where IStream::Seek doesn't? ----
            let raw_len = o.window.min(size - mid.min(size)).min(1024 * 1024) as u32;
            let mid_ref = references
                .iter()
                .find(|(off, _)| *off == mid)
                .map(|(_, r)| r.as_slice());
            p.raw_seek_probe(&id, mid, raw_len.max(1), mid_ref);

            if let Some(h) = handle {
                // ---- windows with ordinary WPD calls between them (the #61 pattern) ---------------
                println!("\n== Two passthrough windows with a folder listing between them ==");
                let w = o
                    .window
                    .min(size.saturating_sub(mid) / 2)
                    .max(1)
                    .min(u32::MAX as u64) as u32;
                let a = p.partial(OP_GET_PARTIAL_OBJECT_64, h, mid, w, o.chunk, false);
                describe("window 1 at the middle", &a, None);
                let t0 = Instant::now();
                match p.children(&parent_id) {
                    Ok(k) => println!(
                        "  listing the parent between windows: {} objects in {:.1} ms",
                        k.len(),
                        ms(t0.elapsed())
                    ),
                    Err(e) => println!("  listing the parent between windows: failed: {e}"),
                }
                let b = p.partial(
                    OP_GET_PARTIAL_OBJECT_64,
                    h,
                    mid + w as u64,
                    w,
                    o.chunk,
                    false,
                );
                describe("window 2, right after window 1", &b, None);

                // ---- throughput ----------------------------------------------------------------
                if o.bench > 0 {
                    let total = o.bench.min(size);
                    println!(
                        "\n== Throughput: first {total} B ({:.1} MiB) ==",
                        total as f64 / MIB
                    );
                    let t0 = Instant::now();
                    match p.open_stream(&id).and_then(|(s, optimal)| {
                        let mut buf = vec![0u8; optimal.max(DEFAULT_CHUNK) as usize];
                        read_up_to(&s, total, &mut buf, false)
                    }) {
                        Ok((_, got)) => println!(
                            "  stream, one pass: {got} B in {:.1} ms, {:.1} MiB/s",
                            ms(t0.elapsed()),
                            mbps(got, t0.elapsed())
                        ),
                        Err(e) => println!("  stream, one pass: failed: {e}"),
                    }
                    let mut chunks: Vec<Option<u32>> =
                        vec![o.chunk, Some(1024 * 1024), Some(4 * 1024 * 1024)];
                    chunks.dedup();
                    for chunk in chunks {
                        let t0 = Instant::now();
                        let mut off = 0u64;
                        let mut windows = 0u32;
                        let mut failed = None;
                        while off < total {
                            let len = o.bench_window.min(total - off).min(u32::MAX as u64) as u32;
                            match p.partial(OP_GET_PARTIAL_OBJECT_64, h, off, len, chunk, false) {
                                Ok(r) if r.response == RC_OK && r.total > 0 => {
                                    off += r.total;
                                    windows += 1;
                                }
                                Ok(r) => {
                                    failed = Some(format!(
                                        "response {:#06x}, {} B",
                                        r.response, r.total
                                    ));
                                    break;
                                }
                                Err(e) => {
                                    failed = Some(e.to_string());
                                    break;
                                }
                            }
                        }
                        let label = match chunk {
                            Some(c) => format!("{} KiB chunks", c / 1024),
                            None => "driver-optimal chunks".to_string(),
                        };
                        match failed {
                            None => println!(
                                "  passthrough 0x95C1, {} B windows, {label}: {windows} windows, {off} B in {:.1} ms, {:.1} MiB/s",
                                o.bench_window,
                                ms(t0.elapsed()),
                                mbps(off, t0.elapsed())
                            ),
                            Some(e) => println!("  passthrough 0x95C1, {label}: failed after {off} B: {e}"),
                        }
                    }
                }

                if o.interleave {
                    interleave(&mut p, &id, h, size, o.chunk);
                }
                if o.early_end {
                    early_end(&mut p, &id, h, size, o.chunk);
                }
            }

            println!("\n== Summary (ms per window; lower is better) ==");
            println!("  offset            stream-reopen   0x95C1      0x101B");
            for r in rows {
                let f = |v: Option<f64>| {
                    v.map(|x| format!("{x:>10.1}"))
                        .unwrap_or_else(|| format!("{:>10}", "-"))
                };
                println!(
                    "  {:<16}  {}   {}  {}",
                    r.offset,
                    f(r.stream_ms),
                    f(r.pass64_ms),
                    f(r.pass32_ms)
                );
            }
            println!("\nDone.");
        }
        Ok(())
    }

    /// Other WPD calls in the middle of a passthrough transfer, and a passthrough call while a
    /// resource stream is open. Either can block or fail; the output says which.
    unsafe fn interleave(p: &mut Probe, id: &str, h: u32, size: u64, chunk: Option<u32>) {
        println!("\n== Interleave: WPD calls inside an open passthrough transfer ==");
        let len = (8 * 1024 * 1024u64).min(size) as u32;
        let res: R<()> = (|| {
            let t = p.begin_read(OP_GET_PARTIAL_OBJECT_64, &[h, 0, 0, len])?;
            let n = chunk
                .or((t.optimal > 0).then_some(t.optimal))
                .unwrap_or(DEFAULT_CHUNK)
                .min(len);
            let first = p.read_chunk(&t, n)?;
            println!("  started an {len} B transfer, read {} B", first.len());

            let idw = wide(id);
            let t0 = Instant::now();
            match p.props.GetValues(PCWSTR(idw.as_ptr()), None) {
                Ok(_) => println!(
                    "  GetValues on the file mid-transfer: ok in {:.1} ms",
                    ms(t0.elapsed())
                ),
                Err(e) => println!(
                    "  GetValues on the file mid-transfer: {} after {:.1} ms",
                    hr_text(&e),
                    ms(t0.elapsed())
                ),
            }
            let t0 = Instant::now();
            match p.open_stream(id).and_then(|(s, _)| {
                let mut buf = vec![0u8; 64 * 1024];
                read_up_to(&s, 64 * 1024, &mut buf, false)
            }) {
                Ok((_, got)) => println!(
                    "  stream open + 64 KiB read mid-transfer: {got} B in {:.1} ms",
                    ms(t0.elapsed())
                ),
                Err(e) => println!(
                    "  stream open + 64 KiB read mid-transfer: failed after {:.1} ms: {e}",
                    ms(t0.elapsed())
                ),
            }

            let mut got = first.len() as u64;
            while got < t.total {
                let c = p.read_chunk(&t, (n as u64).min(t.total - got) as u32)?;
                if c.is_empty() {
                    break;
                }
                got += c.len() as u64;
            }
            let (rc, params) = p.end(&t)?;
            println!(
                "  finished the transfer: {got} of {} B, response {rc:#06x} {params:x?}",
                t.total
            );
            Ok(())
        })();
        if let Err(e) = res {
            println!("  failed: {e}");
        }

        println!("\n== Interleave: passthrough while a resource stream is open ==");
        let res: R<()> = (|| {
            let (s, _) = p.open_stream(id)?;
            let mut buf = vec![0u8; 64 * 1024];
            read_up_to(&s, 64 * 1024, &mut buf, false)?;
            let t0 = Instant::now();
            let r = p.mtp_read(OP_GET_OBJECT_INFO, &[h], None, false);
            match r {
                Ok(r) => println!(
                    "  GetObjectInfo with the stream open: response {:#06x} in {:.1} ms",
                    r.response,
                    ms(t0.elapsed())
                ),
                Err(e) => println!(
                    "  GetObjectInfo with the stream open: failed after {:.1} ms: {e}",
                    ms(t0.elapsed())
                ),
            }
            match read_up_to(&s, 64 * 1024, &mut buf, false) {
                Ok((_, got)) => println!("  reading on from the stream afterwards: {got} B"),
                Err(e) => println!("  reading on from the stream afterwards: failed: {e}"),
            }
            Ok(())
        })();
        if let Err(e) = res {
            println!("  failed: {e}");
        }
    }

    /// End a transfer after one chunk (what a cancel would do), then check the session.
    unsafe fn early_end(p: &mut Probe, id: &str, h: u32, size: u64, chunk: Option<u32>) {
        println!("\n== Early END_DATA_TRANSFER (cancel mid-window) ==");
        let len = (16 * 1024 * 1024u64).min(size) as u32;
        let res: R<()> = (|| {
            let t = p.begin_read(OP_GET_PARTIAL_OBJECT_64, &[h, 0, 0, len])?;
            let n = chunk
                .or((t.optimal > 0).then_some(t.optimal))
                .unwrap_or(DEFAULT_CHUNK)
                .min(len);
            let first = p.read_chunk(&t, n)?;
            println!(
                "  started a {} B transfer, read {} B, ending now",
                t.total,
                first.len()
            );
            let t0 = Instant::now();
            match p.end(&t) {
                Ok((rc, params)) => println!(
                    "  END: response {rc:#06x} {params:x?} in {:.1} ms",
                    ms(t0.elapsed())
                ),
                Err(e) => println!("  END: failed after {:.1} ms: {e}", ms(t0.elapsed())),
            }
            Ok(())
        })();
        if let Err(e) = res {
            println!("  failed: {e}");
        }

        println!("  session health after it:");
        let t0 = Instant::now();
        match p.mtp_read(OP_GET_OBJECT_INFO, &[h], None, false) {
            Ok(r) => println!(
                "    passthrough GetObjectInfo: response {:#06x} in {:.1} ms",
                r.response,
                ms(t0.elapsed())
            ),
            Err(e) => println!(
                "    passthrough GetObjectInfo: failed after {:.1} ms: {e}",
                ms(t0.elapsed())
            ),
        }
        let t0 = Instant::now();
        match p.partial(
            OP_GET_PARTIAL_OBJECT_64,
            h,
            0,
            (64 * 1024u64).min(size) as u32,
            chunk,
            true,
        ) {
            Ok(r) => println!(
                "    passthrough 0x95C1 64 KiB: response {:#06x}, {} B in {:.1} ms",
                r.response,
                r.data.len(),
                ms(t0.elapsed())
            ),
            Err(e) => println!(
                "    passthrough 0x95C1 64 KiB: failed after {:.1} ms: {e}",
                ms(t0.elapsed())
            ),
        }
        let t0 = Instant::now();
        match p.open_stream(id).and_then(|(s, _)| {
            let mut buf = vec![0u8; 64 * 1024];
            read_up_to(&s, 64 * 1024, &mut buf, false)
        }) {
            Ok((_, got)) => println!("    stream 64 KiB: {got} B in {:.1} ms", ms(t0.elapsed())),
            Err(e) => println!(
                "    stream 64 KiB: failed after {:.1} ms: {e}",
                ms(t0.elapsed())
            ),
        }
    }
}
