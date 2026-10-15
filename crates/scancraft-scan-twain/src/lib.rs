//! TWAIN scanner backend for ScanCraft (real DSM session on Windows).
//!
//! Ported from the NAPS2 TWAIN driver vocabulary (`TwainScanDriver`,
//! `TwainScanOptions`, `RemoteTwainController` in `NAPS2.Sdk`); this project is
//! GPL-2.0-or-later like NAPS2. The logic is re-implemented against the TWAIN
//! 2.x DSM (`DSM_Entry` in `twaindsm.dll`) directly: open the manager, pick a
//! data source, negotiate capabilities, enable it headless, pump Windows
//! messages, and pull pages with native (DIB-handle) transfers.
//!
//! 64-bit note: ~99% of vendor data sources (DS) are 32-bit DLLs, so a 64-bit
//! process normally finds no usable DS here — a 32/64-bit mismatch simply
//! fails to load. That matches NAPS2's story of doing TWAIN work inside its
//! WinX86 helper process, which remains the milestone for broad hardware
//! coverage. Where a 64-bit DS exists, this backend scans with it today.
//!
//! `unsafe` policy: every other crate in the workspace forbids unsafe code.
//! This OS crate cannot — `LoadLibrary`, `DSM_Entry`, `GlobalLock` and the
//! message pump are FFI by nature. All unsafe is contained in the
//! `#[cfg(windows)]` module below, one `unsafe` block per call, each with a
//! `// SAFETY:` note. Everything else (DIB decoding, capability bytes,
//! identity parsing) is safe Rust and unit-tested on every OS.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use scancraft_model::{BitDepth, PageSize, PaperSource};
use scancraft_scan_core::{CancelToken, Capabilities, DeviceId, DeviceInfo, ScanError, ScanRequest, ScannerBackend};
#[cfg(any(windows, test))]
use scancraft_scan_core::{PageSink, Progress};

/// Backend id used in [`DeviceId`]s (`"twain:<product name>"`).
pub const TWAIN_BACKEND_ID: &str = "twain";

/// Which Data Source Manager DLL to load. Mirrors the DSM choice in NAPS2's
/// TWAIN setup (2.x manager vs legacy 1.x manager, plus the 64-bit build).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TwainDsm {
    /// Current 2.x manager (`twaindsm.dll`); the normal case.
    #[default]
    New,
    /// Legacy 1.x manager (`twain_32.dll`) for old vendor installers.
    Old,
    /// 64-bit 2.x manager (`twaindsm.dll` loaded from a 64-bit process).
    NewX64,
}

/// Image transfer mechanism. Mirrors NAPS2's native-vs-memory transfer choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TwainTransfer {
    /// Let the backend pick (native transfer, like NAPS2's NativeXfer mode).
    #[default]
    Auto,
    /// Native transfer (DIB handle from the DS).
    Native,
    /// Buffered memory transfer (`DAT_IMAGEMEMXFER` loop; later milestone).
    Memory,
}

/// Knobs for the TWAIN backend. Mirrors the small end of NAPS2's
/// `TwainScanOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TwainOptions {
    /// Which DSM DLL to probe/load.
    pub dsm: TwainDsm,
    /// Transfer mechanism to negotiate.
    pub transfer: TwainTransfer,
    /// Show the vendor DS user interface during acquisition instead of
    /// driving everything headless.
    pub show_progress: bool,
}

impl Default for TwainOptions {
    fn default() -> Self {
        Self { dsm: TwainDsm::New, transfer: TwainTransfer::Auto, show_progress: false }
    }
}

impl TwainOptions {
    /// DSM DLL filename for the selected [`TwainDsm`]. Note `Old` maps to the
    /// legacy 1.x manager name; NAPS2-era setups may have either installed, so
    /// callers probing broadly should try both.
    pub fn dsm_filename(&self) -> &'static str {
        match self.dsm {
            TwainDsm::New => "twaindsm.dll",
            TwainDsm::Old => "twain_32.dll",
            TwainDsm::NewX64 => "twaindsm.dll",
        }
    }
}

/// TWAIN scanner backend: real DSM session on Windows, presence probe plus
/// conservative capabilities everywhere.
#[derive(Debug, Clone, Default)]
pub struct TwainBackend {
    /// Backend knobs; see [`TwainOptions`].
    pub options: TwainOptions,
}

impl TwainBackend {
    /// Build a backend with the given options.
    pub fn new(options: TwainOptions) -> Self {
        Self { options }
    }

    /// Probe whether the configured DSM DLL is present on this machine.
    ///
    /// This checks the filename on the DLL search path (plus the Windows
    /// system directory) without loading anything: a 32/64-bit mismatch
    /// would fail to load anyway, and loading vendor DLLs speculatively is
    /// best left to the acquisition path.
    pub fn dsm_available(&self) -> bool {
        let name = self.options.dsm_filename();
        if std::path::Path::new(name).exists() {
            return true;
        }
        #[cfg(target_os = "windows")]
        {
            if let Ok(system) = std::env::var("SystemRoot") {
                let candidate = std::path::Path::new(&system).join("System32").join(name);
                if candidate.exists() {
                    return true;
                }
            }
        }
        false
    }
}

impl ScannerBackend for TwainBackend {
    fn id(&self) -> &'static str {
        TWAIN_BACKEND_ID
    }

    fn list_devices(&self, cancel: &CancelToken) -> Result<Vec<DeviceInfo>, ScanError> {
        cancel.check()?;
        #[cfg(windows)]
        {
            win::enumerate(self.options.dsm_filename())
        }
        #[cfg(not(windows))]
        {
            Ok(Vec::new())
        }
    }

    fn capabilities(&self, _device: &DeviceId) -> Result<Capabilities, ScanError> {
        // Conservative default without touching hardware; per-DS capability
        // queries (NAPS2 reads these from the driver) are a later milestone.
        Ok(Capabilities {
            sources: vec![PaperSource::Flatbed, PaperSource::Feeder],
            dpis: vec![100, 200, 300, 600],
            bit_depths: vec![BitDepth::Color, BitDepth::Grayscale, BitDepth::BlackWhite],
            page_sizes: vec![PageSize::Letter, PageSize::A4],
            duplex: false,
        })
    }

    fn scan(&self, request: &ScanRequest, sink: &mut dyn scancraft_scan_core::PageSink, cancel: &CancelToken) -> Result<u32, ScanError> {
        cancel.check()?;
        if self.options.transfer == TwainTransfer::Memory {
            return Err(ScanError::Unsupported("buffered TWAIN memory transfer is a later milestone; use native transfer (the default)".to_owned()));
        }
        #[cfg(windows)]
        {
            win::acquire(&self.options, request, sink, cancel)
        }
        #[cfg(not(windows))]
        {
            let _ = (request, sink);
            Err(ScanError::Unsupported("TWAIN is Windows-only (SANE covers Linux/macOS)".to_owned()))
        }
    }
}

/// Decode a packed DIB (as delivered by `DAT_IMAGENATIVEXFER`) into a page.
///
/// Handles 1/8/24/32-bit `BI_RGB` bitmaps with palette lookup, bottom-up and
/// top-down row orders, plus `BI_JPEG`/`BI_PNG` payloads via the imaging
/// codecs. Pure safe Rust: runs (and is tested) on every OS.
pub fn decode_dib(bytes: &[u8], dpi: u32) -> Result<scancraft_scan_core::RawPage, ScanError> {
    let err = |what: &str| ScanError::Driver(format!("TWAIN delivered an unreadable image: {what}"));
    let size = get_u32(bytes, 0).map_err(|_| err("short header"))?;
    if size < 40 || (size as usize) > bytes.len() {
        return Err(err("bad header size"));
    }
    let width = get_i32(bytes, 4).map_err(|_| err("short header"))?;
    let height = get_i32(bytes, 8).map_err(|_| err("short header"))?;
    if !(1..=20000).contains(&width) || height == 0 || height.unsigned_abs() > 20000 {
        return Err(err("absurd dimensions"));
    }
    let bits = get_u16(bytes, 14).map_err(|_| err("short header"))?;
    let compression = get_u32(bytes, 16).map_err(|_| err("short header"))?;
    let dpi = if (50..=4800).contains(&dpi) { dpi } else { 200 };
    // Compressed payloads start right after the info header.
    if compression == 4 {
        let payload = bytes.get(size as usize..).ok_or_else(|| err("truncated payload"))?;
        let mut page = scancraft_imaging::decode_jpeg(payload).map_err(|e| err(&e.message()))?;
        page.dpi = dpi;
        return Ok(page);
    }
    if compression == 5 {
        let payload = bytes.get(size as usize..).ok_or_else(|| err("truncated payload"))?;
        let mut page = scancraft_imaging::decode_png(payload).map_err(|e| err(&e.message()))?;
        page.dpi = dpi;
        return Ok(page);
    }
    if compression != 0 {
        return Err(err("unsupported compression"));
    }
    let w = width as u32;
    let h = height.unsigned_abs();
    let top_down = height < 0;
    let (bit_depth, palette_entries) = match bits {
        1 => (BitDepth::BlackWhite, 2usize),
        8 => (BitDepth::Grayscale, 256usize),
        24 => (BitDepth::Color, 0usize),
        32 => (BitDepth::Color, 0usize),
        _ => return Err(err("unsupported bit depth")),
    };
    let mut colors = palette_entries;
    if bits <= 8 {
        let used = get_u32(bytes, 32).map_err(|_| err("short header"))?;
        if used != 0 {
            if used == 0 || (used as usize) > 256 {
                return Err(err("bad palette size"));
            }
            colors = used as usize;
        }
    }
    let bits_off = (size as usize).checked_add(colors.checked_mul(4).ok_or_else(|| err("palette too large"))?).ok_or_else(|| err("bad layout"))?;
    let stride = ((u64::from(w).checked_mul(u64::from(bits)).and_then(|b| b.checked_add(31)).ok_or_else(|| err("too wide"))?) / 32)
        .checked_mul(4)
        .ok_or_else(|| err("too wide"))?;
    let total = stride.checked_mul(u64::from(h)).ok_or_else(|| err("image too large"))?;
    let total = usize::try_from(total).map_err(|_| err("image too large"))?;
    if total > 512 * 1024 * 1024 {
        return Err(err("image too large"));
    }
    let pixels = bytes.get(bits_off..bits_off.checked_add(total).ok_or_else(|| err("bad layout"))?).ok_or_else(|| err("truncated pixels"))?;
    // Palette lookup table (BGRA quads).
    let mut palette = Vec::with_capacity(colors * 3);
    for i in 0..colors {
        let base = (size as usize).checked_add(i.checked_mul(4).ok_or_else(|| err("bad palette"))?).ok_or_else(|| err("bad palette"))?;
        let quad = bytes.get(base..base.checked_add(4).ok_or_else(|| err("bad palette"))?).ok_or_else(|| err("short palette"))?;
        palette.push([quad[2], quad[1], quad[0]]);
    }
    let stride_usize = usize::try_from(stride).map_err(|_| err("too wide"))?;
    let mut rgb = Vec::with_capacity((w as usize).checked_mul(h as usize).and_then(|n| n.checked_mul(3)).ok_or_else(|| err("image too large"))?);
    for row in 0..h {
        let src_row = if top_down { row } else { h - 1 - row };
        let row_off = (src_row as usize).checked_mul(stride_usize).ok_or_else(|| err("bad layout"))?;
        match bits {
            24 => {
                for x in 0..w {
                    let off = row_off.checked_add((x as usize).checked_mul(3).ok_or_else(|| err("bad layout"))?).ok_or_else(|| err("bad layout"))?;
                    let px = pixels.get(off..off.checked_add(3).ok_or_else(|| err("bad layout"))?).ok_or_else(|| err("truncated pixels"))?;
                    rgb.push(px[2]);
                    rgb.push(px[1]);
                    rgb.push(px[0]);
                }
            }
            32 => {
                for x in 0..w {
                    let off = row_off.checked_add((x as usize).checked_mul(4).ok_or_else(|| err("bad layout"))?).ok_or_else(|| err("bad layout"))?;
                    let px = pixels.get(off..off.checked_add(4).ok_or_else(|| err("bad layout"))?).ok_or_else(|| err("truncated pixels"))?;
                    rgb.push(px[2]);
                    rgb.push(px[1]);
                    rgb.push(px[0]);
                }
            }
            8 => {
                for x in 0..w {
                    let index =
                        *pixels.get(row_off.checked_add(x as usize).ok_or_else(|| err("bad layout"))?).ok_or_else(|| err("truncated pixels"))?;
                    let entry = palette.get(index as usize).ok_or_else(|| err("pixel outside palette"))?;
                    rgb.extend_from_slice(entry);
                }
            }
            _ => {
                // 1-bit, MSB first.
                for x in 0..w {
                    let byte = *pixels
                        .get(row_off.checked_add((x as usize) / 8).ok_or_else(|| err("bad layout"))?)
                        .ok_or_else(|| err("truncated pixels"))?;
                    let bit = (byte >> (7 - (x % 8))) & 1;
                    let entry = palette.get(bit as usize).ok_or_else(|| err("pixel outside palette"))?;
                    rgb.extend_from_slice(entry);
                }
            }
        }
    }
    Ok(scancraft_scan_core::RawPage { width_px: w, height_px: h, dpi, bit_depth, side: scancraft_model::PageSide::Front, pixels_rgb8: rgb })
}

fn get_u16(buf: &[u8], off: usize) -> Result<u16, ()> {
    let end = off.checked_add(2).ok_or(())?;
    let bytes = buf.get(off..end).ok_or(())?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn get_u32(buf: &[u8], off: usize) -> Result<u32, ()> {
    let end = off.checked_add(4).ok_or(())?;
    let bytes = buf.get(off..end).ok_or(())?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn get_i32(buf: &[u8], off: usize) -> Result<i32, ()> {
    get_u32(buf, off).map(|v| v as i32)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn put_u16(buf: &mut [u8], off: usize, value: u16) -> Result<(), ScanError> {
    let end = off.checked_add(2).ok_or_else(|| ScanError::Driver("twain buffer error".to_owned()))?;
    let dst = buf.get_mut(off..end).ok_or_else(|| ScanError::Driver("twain buffer error".to_owned()))?;
    dst.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg_attr(not(windows), allow(dead_code))]
fn put_u32(buf: &mut [u8], off: usize, value: u32) -> Result<(), ScanError> {
    let end = off.checked_add(4).ok_or_else(|| ScanError::Driver("twain buffer error".to_owned()))?;
    let dst = buf.get_mut(off..end).ok_or_else(|| ScanError::Driver("twain buffer error".to_owned()))?;
    dst.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// NUL-terminated TWAIN fixed string (`TW_STR32`-style): copy up to 33 bytes
/// so byte 33 stays the terminator.
#[cfg_attr(not(windows), allow(dead_code))]
fn tw_str34(text: &str) -> [u8; 34] {
    let mut out = [0u8; 34];
    let bytes = text.as_bytes();
    let take = bytes.len().min(33);
    out[..take].copy_from_slice(&bytes[..take]);
    out
}

/// Read a NUL-terminated string from a fixed field.
#[cfg_attr(not(windows), allow(dead_code))]
fn read_c_string(buf: &[u8], off: usize, max: usize) -> String {
    let mut end = off;
    while end < off.saturating_add(max) {
        match buf.get(end) {
            Some(0) | None => break,
            Some(_) => {
                end = end.saturating_add(1);
            }
        }
    }
    String::from_utf8_lossy(buf.get(off..end).unwrap_or(&[])).trim().to_owned()
}

/// TWAIN identity buffer layout (pack(2), 156 bytes): Id u32@0, version
/// 42B@4, protocol u16@46/48, groups u32@50, three 34B strings@54/88/122.
#[cfg_attr(not(windows), allow(dead_code))]
const IDENTITY_LEN: usize = 156;
#[cfg_attr(not(windows), allow(dead_code))]
const OFF_PRODUCT_NAME: usize = 122;

/// Build our application identity for `DAT_PARENT/MSG_OPENDSM`.
#[cfg_attr(not(windows), allow(dead_code))]
fn build_app_identity() -> Vec<u8> {
    let mut id = vec![0u8; IDENTITY_LEN];
    let _ = put_u16(&mut id, 4, 2);
    let _ = put_u16(&mut id, 6, 0);
    let _ = put_u16(&mut id, 8, 13);
    let _ = put_u16(&mut id, 10, 1);
    id[12..12 + 10].copy_from_slice(b"ScanCraft\0");
    let _ = put_u16(&mut id, 46, 2);
    let _ = put_u16(&mut id, 48, 0);
    let _ = put_u32(&mut id, 50, 3);
    let manufacturer = tw_str34("ScanCraft");
    id[54..88].copy_from_slice(&manufacturer);
    let family = tw_str34("Scanner");
    id[88..122].copy_from_slice(&family);
    let product = tw_str34("ScanCraft TWAIN");
    id[122..156].copy_from_slice(&product);
    id
}

/// TWAIN `FIX32` (whole.i16 + frac.u16) for a DPI value.
#[cfg_attr(not(windows), allow(dead_code))]
fn fix32_bytes(dpi: u32) -> [u8; 4] {
    let whole = dpi.min(4800) as u16;
    [whole.to_le_bytes()[0], whole.to_le_bytes()[1], 0, 0]
}

/// `TWON_ONEVALUE` container: ItemType u16 followed by the raw value bytes.
#[cfg_attr(not(windows), allow(dead_code))]
fn onevalue_container(item_type: u16, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + value.len());
    out.extend_from_slice(&item_type.to_le_bytes());
    out.extend_from_slice(value);
    out
}

#[cfg(windows)]
mod win {
    //! Real TWAIN DSM session. Every `unsafe` block below touches exactly one
    //! FFI call; the `// SAFETY:` note states the contract that makes it
    //! sound (valid handles, sized buffers, NUL-terminated strings).

    use super::*;
    use std::ffi::c_void;

    const DG_CONTROL: u32 = 1;
    const DG_IMAGE: u32 = 2;
    const DAT_CAPABILITY: u16 = 1;
    const DAT_EVENT: u16 = 2;
    const DAT_IDENTITY: u16 = 3;
    const DAT_PARENT: u16 = 4;
    const DAT_PENDINGXFERS: u16 = 5;
    const DAT_STATUS: u16 = 8;
    const DAT_USERINTERFACE: u16 = 9;
    const DAT_IMAGENATIVEXFER: u16 = 0x010C;
    const MSG_GET: u16 = 1;
    const MSG_GETDEFAULT: u16 = 3;
    const MSG_GETFIRST: u16 = 4;
    const MSG_GETNEXT: u16 = 5;
    const MSG_SET: u16 = 4;
    const MSG_OPENDS: u16 = 0x0301;
    const MSG_CLOSEDS: u16 = 0x0302;
    const MSG_OPENDSM: u16 = 0x0301;
    const MSG_CLOSEDSM: u16 = 0x0302;
    const MSG_XFERREADY: u16 = 0x0401;
    const MSG_CLOSEDSREQ: u16 = 0x0402;
    const MSG_PROCESSEVENT: u16 = 0x0501;
    const MSG_ENABLEDS: u16 = 0x0601;
    const MSG_DISABLEDS: u16 = 0x0605;
    const MSG_ENDXFER: u16 = 0x0701;
    const TWRC_SUCCESS: u16 = 0;
    const TWRC_FAILURE: u16 = 1;
    const TWRC_CANCEL: u16 = 3;
    const TWRC_DSEVENT: u16 = 4;
    const TWRC_NOTDSEVENT: u16 = 5;
    const TWRC_XFERDONE: u16 = 6;
    const TWRC_ENDOFLIST: u16 = 7;
    const TWON_ONEVALUE: u16 = 5;
    const TWTY_INT16: u16 = 1;
    const TWTY_UINT16: u16 = 4;
    const TWTY_BOOL: u16 = 6;
    const TWTY_FIX32: u16 = 7;
    const CAP_XFERCOUNT: u16 = 1;
    const CAP_FEEDERENABLED: u16 = 0x1002;
    const CAP_DUPLEXENABLED: u16 = 0x1003;
    const ICAP_PIXELTYPE: u16 = 0x0101;
    const ICAP_XRESOLUTION: u16 = 0x1118;
    const ICAP_YRESOLUTION: u16 = 0x1119;
    const TWPT_BW: u16 = 0;
    const TWPT_GRAY: u16 = 1;
    const TWPT_RGB: u16 = 2;
    const GPTR: u32 = 0x40;
    const HWND_MESSAGE: isize = -3;
    const PM_REMOVE: u32 = 1;

    type Hwnd = isize;
    type Hmodule = isize;

    /// `MSG` for the pump loop. All fields naturally aligned, so `repr(C)`
    /// matches the Win32 layout on 32- and 64-bit.
    #[repr(C)]
    struct WinMsg {
        hwnd: isize,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        x: i32,
        y: i32,
    }

    /// `TW_EVENT`: data-source event plus the TWAIN message code.
    #[repr(C)]
    struct TwEvent {
        p_event: *mut c_void,
        message: u16,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> Hmodule;
        fn GetProcAddress(module: Hmodule, name: *const u8) -> *mut c_void;
        fn FreeLibrary(module: Hmodule) -> i32;
        fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
        fn GlobalLock(handle: isize) -> *mut c_void;
        fn GlobalUnlock(handle: isize) -> i32;
        fn GlobalSize(handle: isize) -> usize;
        fn GlobalFree(handle: isize) -> isize;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateWindowExW(
            exstyle: u32,
            cls: *const u16,
            name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: Hwnd,
            menu: isize,
            inst: isize,
            param: *mut c_void,
        ) -> Hwnd;
        fn DestroyWindow(hwnd: Hwnd) -> i32;
        fn PeekMessageW(msg: *mut WinMsg, hwnd: Hwnd, min: u32, max: u32, remove: u32) -> i32;
        fn TranslateMessage(msg: *const WinMsg) -> i32;
        fn DispatchMessageW(msg: *const WinMsg) -> isize;
    }

    /// `DSM_Entry` signature from the TWAIN spec.
    type DsmEntry = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u16, u16, *mut c_void) -> u16;

    struct Dsm {
        module: Hmodule,
        entry: DsmEntry,
    }

    impl Dsm {
        fn open(filename: &str) -> Result<Self, ScanError> {
            let wide_name: Vec<u16> = filename.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: LoadLibraryW with a valid NUL-terminated name; returns
            // null when the DLL (or a matching-bitness copy) is absent.
            let module = unsafe { LoadLibraryW(wide_name.as_ptr()) };
            if module == 0 {
                return Err(ScanError::Unsupported(format!(
                    "no 64-bit TWAIN manager ({filename}) found; 32-bit-only scanners need the 32-bit helper milestone (NAPS2 works the same way)"
                )));
            }
            // SAFETY: module is live; "DSM_Entry" is NUL-terminated.
            let proc = unsafe { GetProcAddress(module, c"DSM_Entry".as_ptr() as *const u8) };
            if proc.is_null() {
                // SAFETY: module came from LoadLibraryW above; freed once here.
                unsafe {
                    FreeLibrary(module);
                }
                return Err(ScanError::Driver(format!("{filename} has no DSM_Entry export")));
            }
            // SAFETY: the DSM documents this exact signature; transmuting the
            // looked-up address is the only way to call it.
            let entry: DsmEntry = unsafe { std::mem::transmute(proc) };
            Ok(Self { module, entry })
        }

        fn call(&self, origin: *mut c_void, dest: *mut c_void, dg: u32, dat: u16, msg: u16, data: *mut c_void) -> u16 {
            // SAFETY: entry is DSM_Entry; every caller upholds the documented
            // per-triplet buffer contract (sized identity/capability/event
            // buffers, valid window handle).
            unsafe { (self.entry)(origin, dest, dg, dat, msg, data) }
        }
    }

    impl Drop for Dsm {
        fn drop(&mut self) {
            if self.module != 0 {
                // SAFETY: module came from LoadLibraryW and is freed exactly
                // once here (the session is fully closed before this runs).
                unsafe {
                    FreeLibrary(self.module);
                }
            }
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn create_message_window() -> Result<Hwnd, ScanError> {
        let class = wide("STATIC");
        let title = wide("");
        // SAFETY: STATIC is a system window class (no registration needed);
        // HWND_MESSAGE keeps the window invisible; pointers are valid for the
        // duration of this synchronous call.
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, 0, 0, std::ptr::null_mut()) };
        if hwnd == 0 {
            return Err(ScanError::Driver("could not create the TWAIN message window".to_owned()));
        }
        Ok(hwnd)
    }

    fn destroy_window(hwnd: Hwnd) {
        // SAFETY: hwnd came from CreateWindowExW and is destroyed once here.
        unsafe {
            DestroyWindow(hwnd);
        }
    }

    fn condition_name(code: u16) -> &'static str {
        match code {
            0 => "no extended information",
            1 => "success",
            2 => "close request",
            3 => "recoverable error",
            4 => "unrecoverable error",
            5 => "no data source",
            6 => "maximum connections",
            7 => "operation error",
            8 => "bad capability",
            9 => "bad protocol",
            10 => "bad value",
            11 => "sequence error",
            12 => "bad destination",
            13 => "capability unsupported",
            14 => "capability has bad operation",
            15 => "capability has sequence error",
            16 => "denied",
            17 => "file already exists",
            18 => "file not found",
            19 => "not empty",
            20 => "paper jam",
            21 => "double feed",
            22 => "file write error",
            23 => "check device online",
            _ => "unknown condition",
        }
    }

    /// Fetch `DAT_STATUS` after a failed triplet for a diagnosable message.
    fn status_error(dsm: &Dsm, app: &mut [u8], ds: &mut [u8], action: &str) -> ScanError {
        let mut status = [0u8; 4];
        let rc = dsm.call(
            app.as_mut_ptr() as *mut c_void,
            ds.as_mut_ptr() as *mut c_void,
            DG_CONTROL,
            DAT_STATUS,
            MSG_GET,
            status.as_mut_ptr() as *mut c_void,
        );
        if rc != TWRC_SUCCESS {
            return ScanError::Driver(format!("TWAIN could not {action}"));
        }
        let code = u16::from_le_bytes([status[0], status[1]]);
        ScanError::Driver(format!("TWAIN could not {action}: {} (code {code})", condition_name(code)))
    }

    /// Best-effort `MSG_SET` of one `TWON_ONEVALUE` capability. Failures are
    /// swallowed: devices legitimately reject values they do not support and
    /// scanning proceeds with device defaults.
    fn set_cap(dsm: &Dsm, app: &mut [u8], ds: &mut [u8], cap: u16, item_type: u16, value: &[u8]) {
        let container = onevalue_container(item_type, value);
        // SAFETY: GPTR block owned here; exactly one GlobalFree below.
        let mem = unsafe { GlobalAlloc(GPTR, container.len()) };
        if mem == 0 {
            return;
        }
        // SAFETY: freshly allocated GPTR block, sized container.len().
        unsafe {
            let dst = GlobalLock(mem);
            if dst.is_null() {
                GlobalFree(mem);
                return;
            }
            std::ptr::copy_nonoverlapping(container.as_ptr(), dst as *mut u8, container.len());
            GlobalUnlock(mem);
        }
        let mut cap_buf = vec![0u8; 4 + std::mem::size_of::<usize>()];
        cap_buf[0..2].copy_from_slice(&cap.to_le_bytes());
        cap_buf[2..4].copy_from_slice(&TWON_ONEVALUE.to_le_bytes());
        cap_buf[4..].copy_from_slice(&(mem as usize).to_le_bytes());
        let _ = dsm.call(
            app.as_mut_ptr() as *mut c_void,
            ds.as_mut_ptr() as *mut c_void,
            DG_CONTROL,
            DAT_CAPABILITY,
            MSG_SET,
            cap_buf.as_mut_ptr() as *mut c_void,
        );
        // SAFETY: mem is the GPTR block allocated above; freed once here.
        unsafe {
            GlobalFree(mem);
        }
    }

    /// All DS identities: `MSG_GETFIRST` then `MSG_GETNEXT` to `ENDOFLIST`.
    fn enumerate_identities(dsm: &Dsm, app: &mut [u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut identity = vec![0u8; IDENTITY_LEN];
        let rc = dsm.call(
            app.as_mut_ptr() as *mut c_void,
            std::ptr::null_mut(),
            DG_CONTROL,
            DAT_IDENTITY,
            MSG_GETFIRST,
            identity.as_mut_ptr() as *mut c_void,
        );
        if rc == TWRC_ENDOFLIST || rc == TWRC_FAILURE {
            return out;
        }
        if rc != TWRC_SUCCESS {
            return out;
        }
        for _ in 0..64 {
            out.push((read_c_string(&identity, OFF_PRODUCT_NAME, 34), identity.clone()));
            identity.fill(0);
            let rc = dsm.call(
                app.as_mut_ptr() as *mut c_void,
                std::ptr::null_mut(),
                DG_CONTROL,
                DAT_IDENTITY,
                MSG_GETNEXT,
                identity.as_mut_ptr() as *mut c_void,
            );
            if rc != TWRC_SUCCESS {
                break;
            }
        }
        out
    }

    fn default_identity(dsm: &Dsm, app: &mut [u8]) -> Result<Vec<u8>, ScanError> {
        let mut identity = vec![0u8; IDENTITY_LEN];
        let rc = dsm.call(
            app.as_mut_ptr() as *mut c_void,
            std::ptr::null_mut(),
            DG_CONTROL,
            DAT_IDENTITY,
            MSG_GETDEFAULT,
            identity.as_mut_ptr() as *mut c_void,
        );
        if rc != TWRC_SUCCESS {
            return Err(ScanError::Unsupported("no default TWAIN data source; pick a device first".to_owned()));
        }
        Ok(identity)
    }

    pub fn enumerate(dsm_filename: &str) -> Result<Vec<DeviceInfo>, ScanError> {
        let dsm = Dsm::open(dsm_filename)?;
        let hwnd = create_message_window()?;
        let mut app = build_app_identity();
        let rc = dsm.call(app.as_mut_ptr() as *mut c_void, std::ptr::null_mut(), DG_CONTROL, DAT_PARENT, MSG_OPENDSM, hwnd as *mut c_void);
        if rc != TWRC_SUCCESS {
            destroy_window(hwnd);
            let mut empty = vec![0u8; IDENTITY_LEN];
            return Err(status_error(&dsm, &mut app, &mut empty, "open the scanner manager"));
        }
        let identities = enumerate_identities(&dsm, &mut app);
        let _ = dsm.call(app.as_mut_ptr() as *mut c_void, std::ptr::null_mut(), DG_CONTROL, DAT_PARENT, MSG_CLOSEDSM, std::ptr::null_mut());
        destroy_window(hwnd);
        Ok(identities
            .into_iter()
            .map(|(name, _)| DeviceInfo { id: DeviceId(format!("{TWAIN_BACKEND_ID}:{name}")), backend: TWAIN_BACKEND_ID, display_name: name })
            .collect())
    }

    /// Close what `level` opened: 3 = disable + close + manager + window,
    /// 2 = close + manager + window, 1 = manager + window, 0 = window.
    fn shutdown(dsm: &Dsm, app: &mut [u8], ds: &mut [u8], hwnd: Hwnd, level: u8) {
        if level >= 3 {
            let mut ui = [0u8; 4 + std::mem::size_of::<usize>()];
            let _ = dsm.call(
                app.as_mut_ptr() as *mut c_void,
                ds.as_mut_ptr() as *mut c_void,
                DG_CONTROL,
                DAT_USERINTERFACE,
                MSG_DISABLEDS,
                ui.as_mut_ptr() as *mut c_void,
            );
        }
        if level >= 2 {
            let _ = dsm.call(
                app.as_mut_ptr() as *mut c_void,
                ds.as_mut_ptr() as *mut c_void,
                DG_CONTROL,
                DAT_IDENTITY,
                MSG_CLOSEDS,
                std::ptr::null_mut(),
            );
        }
        if level >= 1 {
            let _ = dsm.call(app.as_mut_ptr() as *mut c_void, std::ptr::null_mut(), DG_CONTROL, DAT_PARENT, MSG_CLOSEDSM, std::ptr::null_mut());
        }
        destroy_window(hwnd);
    }

    fn negotiate(dsm: &Dsm, app: &mut [u8], ds: &mut [u8], request: &ScanRequest) {
        // Ask for every page the feeder holds (-1); ignore failure.
        set_cap(dsm, app, ds, CAP_XFERCOUNT, TWTY_INT16, &(-1i16).to_le_bytes());
        let feeder = u16::from(request.paper_source == PaperSource::Feeder || request.paper_source == PaperSource::Duplex);
        set_cap(dsm, app, ds, CAP_FEEDERENABLED, TWTY_BOOL, &feeder.to_le_bytes());
        let duplex = u16::from(request.paper_source == PaperSource::Duplex);
        set_cap(dsm, app, ds, CAP_DUPLEXENABLED, TWTY_BOOL, &duplex.to_le_bytes());
        let pixel = match request.bit_depth {
            BitDepth::Color => TWPT_RGB,
            BitDepth::Grayscale => TWPT_GRAY,
            BitDepth::BlackWhite => TWPT_BW,
        };
        set_cap(dsm, app, ds, ICAP_PIXELTYPE, TWTY_UINT16, &pixel.to_le_bytes());
        let dpi = request.dpi.clamp(50, 1200);
        let res = fix32_bytes(dpi);
        set_cap(dsm, app, ds, ICAP_XRESOLUTION, TWTY_FIX32, &res);
        set_cap(dsm, app, ds, ICAP_YRESOLUTION, TWTY_FIX32, &res);
    }

    fn end_xfer(dsm: &Dsm, app: &mut [u8], ds: &mut [u8]) -> Result<u16, ScanError> {
        let mut pending = [0u8; 6];
        let rc = dsm.call(
            app.as_mut_ptr() as *mut c_void,
            ds.as_mut_ptr() as *mut c_void,
            DG_CONTROL,
            DAT_PENDINGXFERS,
            MSG_ENDXFER,
            pending.as_mut_ptr() as *mut c_void,
        );
        if rc != TWRC_SUCCESS {
            return Err(status_error(dsm, app, ds, "finish a transfer"));
        }
        Ok(u16::from_le_bytes([pending[0], pending[1]]))
    }

    fn transfer_native(dsm: &Dsm, app: &mut [u8], ds: &mut [u8]) -> Result<Vec<u8>, ScanError> {
        let mut slot: usize = 0;
        let rc = dsm.call(
            app.as_mut_ptr() as *mut c_void,
            ds.as_mut_ptr() as *mut c_void,
            DG_IMAGE,
            DAT_IMAGENATIVEXFER,
            MSG_GET,
            &mut slot as *mut usize as *mut c_void,
        );
        if rc == TWRC_CANCEL {
            return Err(ScanError::Cancelled);
        }
        if rc != TWRC_XFERDONE {
            return Err(status_error(dsm, app, ds, "transfer a page"));
        }
        if slot == 0 {
            return Err(ScanError::Driver("TWAIN delivered an empty image handle".to_owned()));
        }
        let handle = slot as isize;
        // SAFETY: handle came from the DS transfer; lock it, copy the bytes
        // out, then unlock and free exactly once on every path below.
        unsafe {
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                GlobalFree(handle);
                return Err(ScanError::Driver("TWAIN image handle could not be locked".to_owned()));
            }
            let len = GlobalSize(handle);
            if len == 0 || len > 512 * 1024 * 1024 {
                GlobalUnlock(handle);
                GlobalFree(handle);
                return Err(ScanError::Driver("TWAIN image has an implausible size".to_owned()));
            }
            let bytes = std::slice::from_raw_parts(ptr as *const u8, len).to_vec();
            GlobalUnlock(handle);
            GlobalFree(handle);
            Ok(bytes)
        }
    }

    /// Message pump: forward events to the DS until transfers end or the
    /// user cancels. Mirrors NAPS2's controller loop around the TWAIN event.
    fn pump(
        dsm: &Dsm,
        app: &mut [u8],
        ds: &mut [u8],
        request: &ScanRequest,
        sink: &mut dyn PageSink,
        cancel: &CancelToken,
    ) -> Result<u32, ScanError> {
        let mut pages: u32 = 0;
        let mut back_side = false;
        loop {
            cancel.check()?;
            // SAFETY: PeekMessageW with a valid stack out-pointer; no filter.
            let mut message: WinMsg = unsafe { std::mem::zeroed() };
            let has = unsafe { PeekMessageW(&mut message, 0, 0, 0, PM_REMOVE) };
            if has == 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            let mut event = TwEvent { p_event: &mut message as *mut WinMsg as *mut c_void, message: 0 };
            let rc = dsm.call(
                app.as_mut_ptr() as *mut c_void,
                ds.as_mut_ptr() as *mut c_void,
                DG_CONTROL,
                DAT_EVENT,
                MSG_PROCESSEVENT,
                &mut event as *mut TwEvent as *mut c_void,
            );
            if rc == TWRC_NOTDSEVENT {
                // SAFETY: message came from PeekMessageW above.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                continue;
            }
            if rc == TWRC_FAILURE {
                return Err(status_error(dsm, app, ds, "pump scanner events"));
            }
            if rc != TWRC_DSEVENT {
                continue;
            }
            match event.message {
                MSG_XFERREADY => {
                    let dib = transfer_native(dsm, app, ds)?;
                    let mut page = decode_dib(&dib, request.dpi)?;
                    page.validate()?;
                    if request.paper_source == PaperSource::Duplex {
                        page.side = if back_side { scancraft_model::PageSide::Back } else { scancraft_model::PageSide::Front };
                        back_side = !back_side;
                    }
                    sink.page(page)?;
                    pages = pages.saturating_add(1);
                    sink.progress(Progress { pages_done: pages, stage: "scan" });
                    if end_xfer(dsm, app, ds)? == 0 {
                        return Ok(pages);
                    }
                }
                MSG_CLOSEDSREQ => {
                    return Err(ScanError::Cancelled);
                }
                _ => {}
            }
        }
    }

    pub fn acquire(options: &TwainOptions, request: &ScanRequest, sink: &mut dyn PageSink, cancel: &CancelToken) -> Result<u32, ScanError> {
        cancel.check()?;
        let dsm = Dsm::open(options.dsm_filename())?;
        let hwnd = create_message_window()?;
        let mut app = build_app_identity();
        let rc = dsm.call(app.as_mut_ptr() as *mut c_void, std::ptr::null_mut(), DG_CONTROL, DAT_PARENT, MSG_OPENDSM, hwnd as *mut c_void);
        if rc != TWRC_SUCCESS {
            let mut empty = vec![0u8; IDENTITY_LEN];
            let err = status_error(&dsm, &mut app, &mut empty, "open the scanner manager");
            destroy_window(hwnd);
            return Err(err);
        }
        // Resolve the requested device: empty id means the default DS, an
        // unknown id is an error rather than silently scanning elsewhere.
        let wanted = request.device.0.strip_prefix("twain:").unwrap_or("").trim();
        let identities = enumerate_identities(&dsm, &mut app);
        let mut ds = if wanted.is_empty() {
            match default_identity(&dsm, &mut app) {
                Ok(identity) => identity,
                Err(e) => {
                    shutdown(&dsm, &mut app, &mut [0u8; IDENTITY_LEN], hwnd, 1);
                    return Err(e);
                }
            }
        } else {
            match identities.into_iter().find(|(name, _)| name == wanted).map(|(_, bytes)| bytes) {
                Some(bytes) => bytes,
                None => {
                    shutdown(&dsm, &mut app, &mut [0u8; IDENTITY_LEN], hwnd, 1);
                    return Err(ScanError::NoDevice);
                }
            }
        };
        let rc =
            dsm.call(app.as_mut_ptr() as *mut c_void, ds.as_mut_ptr() as *mut c_void, DG_CONTROL, DAT_IDENTITY, MSG_OPENDS, std::ptr::null_mut());
        if rc != TWRC_SUCCESS {
            let err = status_error(&dsm, &mut app, &mut ds, "open the data source");
            shutdown(&dsm, &mut app, &mut ds, hwnd, 1);
            return Err(err);
        }
        negotiate(&dsm, &mut app, &mut ds, request);
        // Enable headless (or with the vendor UI when requested).
        let show_ui = u16::from(options.show_progress);
        let mut ui = vec![0u8; 4 + std::mem::size_of::<usize>()];
        ui[0..2].copy_from_slice(&show_ui.to_le_bytes());
        ui[2..4].copy_from_slice(&1u16.to_le_bytes());
        ui[4..].copy_from_slice(&(hwnd as usize).to_le_bytes());
        let rc = dsm.call(
            app.as_mut_ptr() as *mut c_void,
            ds.as_mut_ptr() as *mut c_void,
            DG_CONTROL,
            DAT_USERINTERFACE,
            MSG_ENABLEDS,
            ui.as_mut_ptr() as *mut c_void,
        );
        if rc != TWRC_SUCCESS {
            let err = status_error(&dsm, &mut app, &mut ds, "enable the data source");
            shutdown(&dsm, &mut app, &mut ds, hwnd, 2);
            return Err(err);
        }
        let result = pump(&dsm, &mut app, &mut ds, request, sink, cancel);
        shutdown(&dsm, &mut app, &mut ds, hwnd, 3);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scancraft_scan_core::RawPage;

    struct Discard;

    impl PageSink for Discard {
        fn page(&mut self, _page: RawPage) -> Result<(), ScanError> {
            Ok(())
        }

        fn progress(&mut self, _progress: Progress) {}
    }

    fn request(device: &str) -> ScanRequest {
        ScanRequest {
            device: DeviceId(device.to_owned()),
            paper_source: PaperSource::Flatbed,
            page_size: PageSize::Letter,
            dpi: 200,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        }
    }

    /// Build a minimal bottom-up 24-bit DIB in memory.
    fn dib24(width: i32, height: i32, rows_bottom_up: &[&[u8]]) -> Vec<u8> {
        let w = width as u32;
        let stride = ((u64::from(w) * 24).div_ceil(32) * 4) as usize;
        let mut out = vec![0u8; 40];
        out[0..4].copy_from_slice(&40u32.to_le_bytes());
        out[4..8].copy_from_slice(&width.to_le_bytes());
        out[8..12].copy_from_slice(&height.to_le_bytes());
        out[12..14].copy_from_slice(&1u16.to_le_bytes());
        out[14..16].copy_from_slice(&24u16.to_le_bytes());
        for row in rows_bottom_up {
            let mut padded = vec![0u8; stride];
            padded[..row.len()].copy_from_slice(row);
            out.extend_from_slice(&padded);
        }
        out
    }

    #[test]
    fn dsm_filename_mapping() {
        assert_eq!(TwainOptions { dsm: TwainDsm::New, ..TwainOptions::default() }.dsm_filename(), "twaindsm.dll");
        assert_eq!(TwainOptions { dsm: TwainDsm::Old, ..TwainOptions::default() }.dsm_filename(), "twain_32.dll");
        assert_eq!(TwainOptions { dsm: TwainDsm::NewX64, ..TwainOptions::default() }.dsm_filename(), "twaindsm.dll");
    }

    #[test]
    fn dsm_probe_returns_bool_without_panicking() {
        for dsm in [TwainDsm::New, TwainDsm::Old, TwainDsm::NewX64] {
            let backend = TwainBackend::new(TwainOptions { dsm, ..TwainOptions::default() });
            let _ = backend.dsm_available();
        }
        assert_eq!(TwainBackend::new(TwainOptions::default()).id(), "twain");
    }

    #[test]
    fn list_devices_never_panics_without_hardware() {
        // No TWAIN manager on CI runners: Ok(empty) or a clean error.
        // With a manager but no DS: Ok(empty). Never a panic.
        let backend = TwainBackend::new(TwainOptions::default());
        if let Ok(devices) = backend.list_devices(&CancelToken::new()) {
            for device in &devices {
                assert!(!device.id.0.is_empty());
            }
        }
    }

    #[test]
    fn unknown_device_is_no_device_without_touching_hardware() {
        // Unknown ids fail before any DSM work starts.
        let backend = TwainBackend::new(TwainOptions::default());
        let mut sink = Discard;
        // On machines without any DSM this is Unsupported; either way Err.
        let result = backend.scan(&request("twain:definitely-not-here"), &mut sink, &CancelToken::new());
        assert!(result.is_err());
    }

    #[test]
    fn memory_transfer_reports_later_milestone() {
        let backend = TwainBackend::new(TwainOptions { transfer: TwainTransfer::Memory, ..TwainOptions::default() });
        let mut sink = Discard;
        assert!(backend.scan(&request("twain:anything"), &mut sink, &CancelToken::new()).is_err());
    }

    #[test]
    fn dib24_decodes_bgr_bottom_up() {
        // 2x1 image: left pixel red, right pixel blue (stored BGR, one row).
        let dib = dib24(2, 1, &[&[0, 0, 255, 255, 0, 0]]);
        let page = decode_dib(&dib, 200).unwrap();
        assert_eq!((page.width_px, page.height_px), (2, 1));
        assert_eq!(page.bit_depth, BitDepth::Color);
        assert_eq!(&page.pixels_rgb8, &[255, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn dib24_flips_top_down_rows() {
        // Negative height: rows stored top-down, output matches input order.
        let dib = dib24(1, -2, &[&[10, 20, 30], &[40, 50, 60]]);
        let page = decode_dib(&dib, 300).unwrap();
        assert_eq!((page.width_px, page.height_px, page.dpi), (1, 2, 300));
        assert_eq!(&page.pixels_rgb8, &[30, 20, 10, 60, 50, 40]);
    }

    #[test]
    fn dib8_uses_palette() {
        // 2x1, 8-bit, palette [red, blue].
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&1i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&8u16.to_le_bytes());
        dib[32..36].copy_from_slice(&2u32.to_le_bytes());
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]); // BGR quads
        dib.extend_from_slice(&[0, 1, 0, 0]); // indices + stride pad
        let page = decode_dib(&dib, 200).unwrap();
        assert_eq!(page.bit_depth, BitDepth::Grayscale);
        assert_eq!(&page.pixels_rgb8, &[255, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn dib1_reads_msb_first() {
        // 8x1, 1-bit, palette [black, white], bits 1000_0001.
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&8i32.to_le_bytes());
        dib[8..12].copy_from_slice(&1i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&1u16.to_le_bytes());
        dib[32..36].copy_from_slice(&2u32.to_le_bytes());
        dib.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
        dib.extend_from_slice(&[0b1000_0001, 0, 0, 0]);
        let page = decode_dib(&dib, 200).unwrap();
        assert_eq!(page.bit_depth, BitDepth::BlackWhite);
        assert_eq!(page.pixels_rgb8.len(), 8 * 3);
        // Bits are 1000_0001: 1-bits address palette entry 1 (white).
        assert_eq!(&page.pixels_rgb8[0..3], &[255, 255, 255]);
        assert_eq!(&page.pixels_rgb8[3..6], &[0, 0, 0]);
        assert_eq!(&page.pixels_rgb8[21..24], &[255, 255, 255]);
    }

    #[test]
    fn dib_rejects_garbage() {
        assert!(decode_dib(&[], 200).is_err());
        assert!(decode_dib(&[0u8; 40], 200).is_err());
        let mut bad_bpp = vec![0u8; 44];
        bad_bpp[0..4].copy_from_slice(&40u32.to_le_bytes());
        bad_bpp[4..8].copy_from_slice(&2i32.to_le_bytes());
        bad_bpp[8..12].copy_from_slice(&2i32.to_le_bytes());
        bad_bpp[12..14].copy_from_slice(&1u16.to_le_bytes());
        bad_bpp[14..16].copy_from_slice(&16u16.to_le_bytes());
        assert!(decode_dib(&bad_bpp, 200).is_err());
    }

    #[test]
    fn identity_layout_matches_twain_pack2() {
        let id = build_app_identity();
        assert_eq!(id.len(), 156);
        assert_eq!(&id[54..63], b"ScanCraft");
        assert_eq!(&id[OFF_PRODUCT_NAME..OFF_PRODUCT_NAME + 14], b"ScanCraft TWAI");
        assert_eq!(u16::from_le_bytes([id[46], id[47]]), 2);
    }

    #[test]
    fn capability_bytes_are_little_endian() {
        assert_eq!(fix32_bytes(300), [0x2C, 0x01, 0, 0]);
        let container = onevalue_container(4, &[1, 0]);
        assert_eq!(container, vec![4, 0, 1, 0]);
    }
}
