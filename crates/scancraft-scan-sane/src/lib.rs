//! SANE scanner backend for ScanCraft (stub for now).
//!
//! Ported from the NAPS2 SANE driver vocabulary (`SaneScanDriver`,
//! `SaneOptions` in `NAPS2.Sdk`); this project is GPL-2.0-or-later like NAPS2.
//!
//! Parity plan vs NAPS2 `SaneScanDriver`: load `libsane.so.1` (P/Invoke
//! equivalent), `sane_init` once per worker lifetime (see
//! [`SaneOptions::keep_initialized`]), enumerate with `sane_get_devices`,
//! match string/numeric options the way NAPS2's option matcher does
//! (resolution, mode, source), then `sane_start` + `sane_read` frames into a
//! page. Until that milestone lands, every method reports `Unsupported` on any
//! OS so callers can fall back to eSCL network scanning.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use scancraft_scan_core::{CancelToken, Capabilities, DeviceId, DeviceInfo, ScanError, ScanRequest, ScannerBackend};

/// Backend id used in [`DeviceId`]s once enumeration lands (`"sane:<name>"`).
pub const SANE_BACKEND_ID: &str = "sane";

/// Knobs for the SANE backend. Mirrors the small end of NAPS2's `SaneOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SaneOptions {
    /// Keep `sane_init` alive across scans (NAPS2 worker behavior: the SANE
    /// worker process initializes once and reuses the handle because some
    /// backends misbehave across init/exit cycles). No effect until the
    /// libsane milestone lands.
    pub keep_initialized: bool,
}

impl SaneOptions {
    /// Build options with an explicit worker-lifetime flag.
    pub fn new(keep_initialized: bool) -> Self {
        Self { keep_initialized }
    }
}

/// SANE scanner backend. Currently a stub: every method returns `Unsupported`
/// on any OS (no `libsane` calls yet, no hardware touched).
#[derive(Debug, Clone, Default)]
pub struct SaneBackend {
    /// Backend knobs; see [`SaneOptions`].
    pub options: SaneOptions,
}

impl SaneBackend {
    /// Build a backend with the given options.
    pub fn new(options: SaneOptions) -> Self {
        Self { options }
    }
}

/// Milestone-pending error: Linux/macOS name the missing libsane work and
/// point at eSCL; other OSs report that SANE does not apply there.
fn unavailable() -> ScanError {
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        ScanError::Unsupported("SANE support needs libsane; milestone pending — use eSCL network scanning meanwhile".to_owned())
    } else {
        ScanError::Unsupported("SANE is Linux/macOS-only".to_owned())
    }
}

impl ScannerBackend for SaneBackend {
    fn id(&self) -> &'static str {
        SANE_BACKEND_ID
    }

    fn list_devices(&self, cancel: &CancelToken) -> Result<Vec<DeviceInfo>, ScanError> {
        cancel.check()?;
        Err(unavailable())
    }

    fn capabilities(&self, _device: &DeviceId) -> Result<Capabilities, ScanError> {
        Err(unavailable())
    }

    fn scan(&self, _request: &ScanRequest, _sink: &mut dyn scancraft_scan_core::PageSink, cancel: &CancelToken) -> Result<u32, ScanError> {
        cancel.check()?;
        Err(unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scancraft_model::{BitDepth, PageSize, PaperSource};
    use scancraft_scan_core::{PageSink, Progress, RawPage};

    struct Discard;

    impl PageSink for Discard {
        fn page(&mut self, _page: RawPage) -> Result<(), ScanError> {
            Ok(())
        }

        fn progress(&mut self, _progress: Progress) {}
    }

    #[test]
    fn options_default_is_not_kept() {
        assert!(!SaneOptions::default().keep_initialized);
        assert_eq!(SaneBackend::new(SaneOptions::default()).id(), "sane");
    }

    #[test]
    fn all_methods_unsupported_on_any_os() {
        let backend = SaneBackend::default();
        let cancel = CancelToken::new();
        assert!(matches!(backend.list_devices(&cancel), Err(ScanError::Unsupported(_))));
        assert!(matches!(backend.capabilities(&DeviceId("sane:anything".to_owned())), Err(ScanError::Unsupported(_))));
        let request = ScanRequest {
            device: DeviceId("sane:anything".to_owned()),
            paper_source: PaperSource::Flatbed,
            page_size: PageSize::Letter,
            dpi: 200,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        };
        let mut sink = Discard;
        assert!(matches!(backend.scan(&request, &mut sink, &cancel), Err(ScanError::Unsupported(_))));
    }
}
