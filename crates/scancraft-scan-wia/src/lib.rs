//! WIA (Windows Image Acquisition) scanner backend for ScanCraft.
//!
//! Ported from the NAPS2 WIA driver vocabulary (`WiaScanDriver` / `WiaScanOptions`
//! in `NAPS2.Sdk`); this project is GPL-2.0-or-later like NAPS2. The logic is
//! re-implemented against the WIA Automation `WIA.DeviceManager` COM object
//! driven through `powershell.exe`, so no third-party Rust bindings are needed.
//!
//! Parity plan vs NAPS2 `WiaScanDriver`:
//! - device enumeration mirrors `GetDevices`: walk `DeviceManager.DeviceInfos`
//!   and read each `DeviceID` + `Name` property, printing `DEVICEID|NAME` lines;
//! - scanning mirrors the flatbed path of `Scan`: connect to the device, take
//!   the first (`Items.Item(1)`) item, set the 6147/6148 resolution properties
//!   best-effort, then `Transfer()` the image into a temp file and decode it.
//! - Simplified for this milestone: no ADF/duplex item selection (NAPS2 walks
//!   feeder items and handles duplex pairs), no native WIA UI
//!   (`use_native_ui` is ignored), single-page transfers only.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use scancraft_model::{BitDepth, PageSize, PaperSource};
use scancraft_scan_core::{CancelToken, Capabilities, DeviceId, DeviceInfo, PageSink, ScanError, ScanRequest, ScannerBackend};
#[cfg(windows)]
use scancraft_scan_core::{Progress, RawPage};
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Backend id used in [`DeviceId`]s (`"wia:<native device id>"`).
pub const WIA_BACKEND_ID: &str = "wia";

/// Which WIA Automation generation to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WiaVersion {
    /// Pick whatever `WIA.DeviceManager` resolves to (the normal case).
    #[default]
    Auto,
    /// Legacy WIA 1.0 behavior quirks (reserved for future gating).
    V1,
    /// WIA 2.0 behavior (reserved for future gating).
    V2,
}

/// Knobs for the WIA backend. Mirrors the small end of NAPS2's `WiaScanOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WiaOptions {
    /// WIA generation to target; see [`WiaVersion`].
    pub version: WiaVersion,
    /// Apply the NAPS2 "offset width" quirk when computing the scan area.
    /// Stored for a later milestone; the current PowerShell transfer path
    /// always captures the device default area.
    pub offset_width: bool,
}

impl WiaOptions {
    /// COM ProgID used for enumeration and transfer.
    ///
    /// WIA Automation exposes a single `DeviceManager` ProgID, so all variants
    /// currently resolve to it; the variants exist so generation-specific
    /// quirks (property ids, [`WiaOptions::offset_width`]) can be gated here
    /// later without changing call sites.
    #[cfg(windows)]
    fn device_manager_progid(&self) -> &'static str {
        match self.version {
            WiaVersion::Auto | WiaVersion::V1 | WiaVersion::V2 => "WIA.DeviceManager",
        }
    }
}

/// WIA scanner backend. All hardware work is done by spawning `powershell.exe`.
#[derive(Debug, Clone, Default)]
pub struct WiaBackend {
    /// Backend knobs; see [`WiaOptions`].
    pub options: WiaOptions,
}

impl WiaBackend {
    /// Build a backend with the given options.
    pub fn new(options: WiaOptions) -> Self {
        Self { options }
    }

    #[cfg(windows)]
    fn list_devices_windows(&self, cancel: &CancelToken) -> Result<Vec<DeviceInfo>, ScanError> {
        let stdout = run_powershell(&list_script(self.options.device_manager_progid()), cancel)?;
        let mut devices = Vec::new();
        for (id, name) in parse_device_lines(&stdout) {
            cancel.check()?;
            devices.push(DeviceInfo { id: DeviceId(format!("{WIA_BACKEND_ID}:{id}")), backend: WIA_BACKEND_ID, display_name: name });
        }
        Ok(devices)
    }

    #[cfg(windows)]
    fn scan_windows(&self, request: &ScanRequest, sink: &mut dyn PageSink, cancel: &CancelToken) -> Result<u32, ScanError> {
        // Mirrors the NAPS2 `WiaScanDriver.Scan` flatbed path:
        //  1. connect via DeviceManager using the native device id;
        //  2. grab `Items.Item(1)` (NAPS2 selects feeder/duplex items here);
        //  3. set 6147/6148 (horizontal/vertical resolution) best-effort;
        //  4. `Transfer()` the image and `SaveFile()` it to a temp path;
        //  5. decode + validate + deliver with a progress report.
        let native = native_device_id(&request.device);
        let out_path = unique_temp_bmp();
        let script = self.build_scan_script(native, request.dpi, &out_path);
        let _printed = run_powershell(&script, cancel)?;
        cancel.check()?;
        let bytes = std::fs::read(&out_path).map_err(|e| ScanError::Io(cap_msg(&format!("could not read WIA transfer file: {e}"))))?;
        // Best-effort cleanup of the temp transfer file.
        let _ = std::fs::remove_file(&out_path);
        cancel.check()?;
        let mut page = decode_transfer(&bytes)?;
        page.dpi = request.dpi;
        page.validate()?;
        cancel.check()?;
        sink.page(page)?;
        sink.progress(Progress { pages_done: 1, stage: "scan" });
        Ok(1)
    }

    /// Build the transfer script (method so the version ProgID is honoured).
    #[cfg(windows)]
    fn build_scan_script(&self, device_id: &str, dpi: u32, out_path: &std::path::Path) -> String {
        let progid = self.options.device_manager_progid();
        let dev = ps_escape(device_id);
        let out = ps_escape(&out_path.to_string_lossy());
        // NAPS2 sets resolution on the item properties; out-of-range requests
        // are skipped here (the device default applies) instead of failing.
        let set_dpi = if (50..=4800).contains(&dpi) {
            format!("try {{ $item.Properties.Item(6147).Value = {dpi} }} catch {{}}\ntry {{ $item.Properties.Item(6148).Value = {dpi} }} catch {{}}")
        } else {
            String::new()
        };
        format!(
            "$ErrorActionPreference = 'Stop'\n$m = New-Object -ComObject {progid}\n$deviceId = '{dev}'\n$outPath = '{out}'\n$dev = $null\nforeach ($d in $m.DeviceInfos) {{ if ($d.DeviceID -eq $deviceId) {{ $dev = $d.Connect(); break }} }}\nif ($null -eq $dev) {{ Write-Error 'device not found'; exit 1 }}\n$item = $dev.Items.Item(1)\n{set_dpi}\n$img = $item.Transfer()\n$img.SaveFile($outPath)\nWrite-Output $outPath"
        )
    }
}

impl ScannerBackend for WiaBackend {
    fn id(&self) -> &'static str {
        WIA_BACKEND_ID
    }

    fn list_devices(&self, cancel: &CancelToken) -> Result<Vec<DeviceInfo>, ScanError> {
        cancel.check()?;
        #[cfg(not(windows))]
        {
            // No WIA off Windows.
            Ok(Vec::new())
        }
        #[cfg(windows)]
        {
            self.list_devices_windows(cancel)
        }
    }

    fn capabilities(&self, _device: &DeviceId) -> Result<Capabilities, ScanError> {
        // Conservative default without touching hardware (matches the mock
        // shape; real per-device WIA property queries are a later milestone).
        Ok(Capabilities {
            sources: vec![PaperSource::Flatbed, PaperSource::Feeder],
            dpis: vec![100, 200, 300, 600],
            bit_depths: vec![BitDepth::Color, BitDepth::Grayscale, BitDepth::BlackWhite],
            page_sizes: vec![PageSize::Letter, PageSize::A4],
            duplex: false,
        })
    }

    fn scan(&self, request: &ScanRequest, sink: &mut dyn PageSink, cancel: &CancelToken) -> Result<u32, ScanError> {
        cancel.check()?;
        #[cfg(not(windows))]
        {
            // Keep params referenced so the build stays warning-free on every OS.
            let _ = (request, sink);
            Err(ScanError::Unsupported("WIA is Windows-only".to_owned()))
        }
        #[cfg(windows)]
        {
            self.scan_windows(request, sink, cancel)
        }
    }
}

/// Parse `DEVICEID|NAME` lines from the enumeration script: split on the first
/// `|`, trim both sides, skip empties and malformed lines.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_device_lines(output: &str) -> Vec<(String, String)> {
    let mut parsed = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '|');
        let id = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").trim();
        if id.is_empty() || name.is_empty() {
            continue;
        }
        parsed.push((id.to_owned(), name.to_owned()));
    }
    parsed
}

/// Strip the `"wia:"` prefix list_devices adds, falling back to the raw id so
/// caller-supplied native ids keep working.
#[cfg(windows)]
fn native_device_id(device: &DeviceId) -> &str {
    device.0.strip_prefix("wia:").unwrap_or(device.0.as_str())
}

/// Escape a string for embedding in a single-quoted PowerShell literal.
#[cfg(windows)]
fn ps_escape(raw: &str) -> String {
    raw.replace('\'', "''")
}

/// Enumeration script: print one `DEVICEID|NAME` line per WIA device.
#[cfg(windows)]
fn list_script(progid: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'\n$m = New-Object -ComObject {progid}\nforeach ($d in $m.DeviceInfos) {{ $nm = ''; try {{ $nm = [string]$d.Properties.Item('Name').Value }} catch {{}}; Write-Output ($d.DeviceID + '|' + $nm) }}"
    )
}

/// Run `powershell -NoProfile -NonInteractive -Command <script>` and return
/// stdout. Any failure (spawn, non-zero exit) becomes a capped Driver error.
#[cfg(windows)]
fn run_powershell(script: &str, cancel: &CancelToken) -> Result<String, ScanError> {
    cancel.check()?;
    let output = std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(script)
        .output()
        .map_err(|e| ScanError::Driver(cap_msg(&format!("could not start powershell: {e}"))))?;
    cancel.check()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let msg = if detail.is_empty() { format!("powershell exited with status {}", output.status) } else { format!("powershell failed: {detail}") };
        return Err(ScanError::Driver(cap_msg(&msg)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(windows)]
static TRANSFER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Process-unique temp path for the `Transfer()`/`SaveFile()` round-trip.
#[cfg(windows)]
fn unique_temp_bmp() -> std::path::PathBuf {
    let n = TRANSFER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("scancraft-wia-{}-{n}.bmp", std::process::id());
    std::env::temp_dir().join(name)
}

/// Decode the transferred bytes. WIA `Transfer()` usually yields BMP; fall back
/// to JPEG then PNG, keeping the last error.
#[cfg(windows)]
fn decode_transfer(bytes: &[u8]) -> Result<RawPage, ScanError> {
    match scancraft_imaging::decode_bmp(bytes) {
        Ok(page) => Ok(page),
        Err(_bmp_err) => match scancraft_imaging::decode_jpeg(bytes) {
            Ok(page) => Ok(page),
            Err(_jpeg_err) => scancraft_imaging::decode_png(bytes),
        },
    }
}

/// Cap driver-facing messages so a chatty PowerShell stderr cannot flood logs.
#[cfg(windows)]
fn cap_msg(msg: &str) -> String {
    const MAX: usize = 500;
    if msg.len() <= MAX { msg.to_owned() } else { msg.chars().take(MAX).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    use scancraft_scan_core::{Progress, RawPage};

    #[cfg(not(windows))]
    struct Discard;

    #[cfg(not(windows))]
    impl PageSink for Discard {
        fn page(&mut self, _page: RawPage) -> Result<(), ScanError> {
            Ok(())
        }

        fn progress(&mut self, _progress: Progress) {}
    }

    #[test]
    fn options_default_is_auto() {
        let options = WiaOptions::default();
        assert_eq!(options.version, WiaVersion::Auto);
        assert!(!options.offset_width);
        assert_eq!(WiaBackend::new(options).id(), "wia");
    }

    #[test]
    fn parses_device_lines() {
        let output = "WIA-TEST-1|My Scanner\r\n\r\nMALFORMED\n|no-id\nid-only|\n  WIA-2 | Second Scanner  \n";
        assert_eq!(
            parse_device_lines(output),
            vec![("WIA-TEST-1".to_owned(), "My Scanner".to_owned()), ("WIA-2".to_owned(), "Second Scanner".to_owned()),]
        );
        assert!(parse_device_lines("").is_empty());
    }

    #[test]
    fn list_devices_never_panics_without_hardware() {
        let backend = WiaBackend::new(WiaOptions::default());
        // With no scanner attached this is Ok (empty or devices) or a Driver
        // error; it must never panic on any OS.
        match backend.list_devices(&CancelToken::new()) {
            Ok(devices) => {
                for device in &devices {
                    assert!(!device.id.0.is_empty());
                }
            }
            Err(err) => {
                let _ = err.message();
            }
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn scan_is_unsupported_off_windows() {
        use scancraft_model::{BitDepth, PageSize, PaperSource};

        let backend = WiaBackend::new(WiaOptions::default());
        let request = ScanRequest {
            device: DeviceId("wia:anything".to_owned()),
            paper_source: PaperSource::Flatbed,
            page_size: PageSize::Letter,
            dpi: 200,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        };
        let mut sink = Discard;
        assert_eq!(backend.scan(&request, &mut sink, &CancelToken::new()), Err(ScanError::Unsupported("WIA is Windows-only".to_owned())));
    }
}
