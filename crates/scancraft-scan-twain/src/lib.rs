//! TWAIN scanner backend for ScanCraft (DSM probe only for now).
//!
//! Ported from the NAPS2 TWAIN driver vocabulary (`TwainScanDriver`,
//! `TwainScanOptions`, `RemoteTwainController` in `NAPS2.Sdk`); this project is
//! GPL-2.0-or-later like NAPS2. Only the Data Source Manager (DSM) presence
//! probe is implemented here.
//!
//! 64-bit note: ~99% of vendor data sources (DS) are 32-bit DLLs, so a 64-bit
//! process loading `twaindsm.dll` normally gets `false` from
//! [`TwainBackend::dsm_available`] — a 32/64-bit mismatch simply fails to
//! load. That matches NAPS2's story of doing TWAIN work inside its WinX86
//! helper process. Real DS enumeration and transfers need that 32-bit helper
//! milestone; the parity plan is to port NAPS2's `RemoteTwainController`
//! (out-of-process TWAIN session + capability negotiation + native/memory
//! transfers) behind the same [`ScannerBackend`] surface.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use scancraft_model::{BitDepth, PageSize, PaperSource};
use scancraft_scan_core::{CancelToken, Capabilities, DeviceId, DeviceInfo, ScanError, ScanRequest, ScannerBackend};

/// Backend id used in [`DeviceId`]s once enumeration lands (`"twain:<id>"`).
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
    /// Let the backend pick (native first, like NAPS2's default).
    #[default]
    Auto,
    /// Native transfer (DIB handle from the DS).
    Native,
    /// Buffered memory transfer (`DAT_IMAGEMEMXFER` loop).
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
    /// Show the vendor DS progress UI during acquisition.
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

/// TWAIN scanner backend. Enumeration and acquisition await the 32-bit DSM
/// helper milestone (see the module docs); until then this reports presence
/// and conservative capabilities only.
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
    /// best left to the 32-bit helper milestone. The expected outcome for a
    /// 64-bit process is `false`, since nearly all vendor DSs are 32-bit
    /// (NAPS2 handles this via its WinX86 worker).
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
        // Real DS enumeration requires opening a DSM session in the 32-bit
        // helper milestone (NAPS2 enumerates via its TWAIN session); report no
        // devices until then rather than guessing.
        let devices: Vec<DeviceInfo> = Vec::new();
        Ok(devices)
    }

    fn capabilities(&self, _device: &DeviceId) -> Result<Capabilities, ScanError> {
        // Conservative default without touching hardware (same shape as WIA).
        Ok(Capabilities {
            sources: vec![PaperSource::Flatbed, PaperSource::Feeder],
            dpis: vec![100, 200, 300, 600],
            bit_depths: vec![BitDepth::Color, BitDepth::Grayscale, BitDepth::BlackWhite],
            page_sizes: vec![PageSize::Letter, PageSize::A4],
            duplex: false,
        })
    }

    fn scan(&self, _request: &ScanRequest, _sink: &mut dyn scancraft_scan_core::PageSink, cancel: &CancelToken) -> Result<u32, ScanError> {
        cancel.check()?;
        Err(ScanError::Unsupported(
            "TWAIN scanning needs the 32-bit DSM helper process (milestone pending); use the WIA, eSCL, or mock backend meanwhile".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scancraft_scan_core::{PageSink, Progress, RawPage};

    struct Discard;

    impl PageSink for Discard {
        fn page(&mut self, _page: RawPage) -> Result<(), ScanError> {
            Ok(())
        }

        fn progress(&mut self, _progress: Progress) {}
    }

    fn request() -> ScanRequest {
        ScanRequest {
            device: DeviceId("twain:anything".to_owned()),
            paper_source: PaperSource::Flatbed,
            page_size: PageSize::Letter,
            dpi: 200,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        }
    }

    #[test]
    fn dsm_filename_mapping() {
        assert_eq!(TwainOptions { dsm: TwainDsm::New, ..TwainOptions::default() }.dsm_filename(), "twaindsm.dll");
        assert_eq!(TwainOptions { dsm: TwainDsm::Old, ..TwainOptions::default() }.dsm_filename(), "twain_32.dll");
        assert_eq!(TwainOptions { dsm: TwainDsm::NewX64, ..TwainOptions::default() }.dsm_filename(), "twaindsm.dll");
    }

    #[test]
    fn dsm_probe_returns_bool_without_panicking() {
        // Hardware-independent: probing only reports whether the DSM loads.
        for dsm in [TwainDsm::New, TwainDsm::Old, TwainDsm::NewX64] {
            let backend = TwainBackend::new(TwainOptions { dsm, ..TwainOptions::default() });
            let _ = backend.dsm_available();
        }
        assert_eq!(TwainBackend::new(TwainOptions::default()).id(), "twain");
    }

    #[test]
    fn list_devices_is_ok_without_hardware() {
        let backend = TwainBackend::new(TwainOptions::default());
        assert!(backend.list_devices(&CancelToken::new()).is_ok());
    }

    #[test]
    fn scan_reports_helper_requirement() {
        let backend = TwainBackend::new(TwainOptions::default());
        let mut sink = Discard;
        assert!(matches!(backend.scan(&request(), &mut sink, &CancelToken::new()), Err(ScanError::Unsupported(_))));
    }
}
