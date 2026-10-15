//! OS-neutral scanner contract. Concrete backends (mock now, WIA/eSCL/TWAIN
//! later) implement [`ScannerBackend`]; the engine depends only on this crate.
//! No OS types may leak here — backends map native errors onto [`ScanError`].
//!
//! Mirrors `NAPS2.Sdk/Scan/Internal/IScanDriver.cs` (`GetDevices`/`GetCaps`/
//! `Scan`) plus the bridge/cancel/error vocabulary around it.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use scancraft_model::{BitDepth, PageSide, PageSize, PaperSource, ScanProfile};

/// Stable device identifier: `"<backend>:<native id>"`, e.g. `"mock:flatbed"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);

/// A discoverable scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub backend: &'static str,
    pub display_name: String,
}

/// What a device claims it can do (approximate until a real caps query runs).
#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
    pub sources: Vec<PaperSource>,
    pub dpis: Vec<u32>,
    pub bit_depths: Vec<BitDepth>,
    pub page_sizes: Vec<PageSize>,
    pub duplex: bool,
}

/// Backend-neutral scan request derived from a [`ScanProfile`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScanRequest {
    pub device: DeviceId,
    pub paper_source: PaperSource,
    pub page_size: PageSize,
    pub dpi: u32,
    pub bit_depth: BitDepth,
    pub brightness: i32,
    pub contrast: i32,
}

impl ScanRequest {
    /// Build from a profile. Values the backend cannot honour are reported
    /// by the backend itself (post-processing applies the rest).
    pub fn from_profile(profile: &ScanProfile) -> Result<Self, ScanError> {
        profile.validate().map_err(|e| ScanError::Unsupported(e.to_string()))?;
        if profile.device_id.is_empty() {
            return Err(ScanError::NoDevice);
        }
        Ok(Self {
            device: DeviceId(profile.device_id.clone()),
            paper_source: profile.paper_source.to_paper_source(),
            page_size: profile.page_size,
            dpi: profile.dpi,
            bit_depth: profile.bit_depth,
            brightness: profile.brightness,
            contrast: profile.contrast,
        })
    }
}

/// One scanned page delivered by a backend (inline RGB8 for now; real
/// backends will stream to a session cache and hand over a key instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPage {
    pub width_px: u32,
    pub height_px: u32,
    pub dpi: u32,
    pub bit_depth: BitDepth,
    pub side: PageSide,
    pub pixels_rgb8: Vec<u8>,
}

impl RawPage {
    /// Reject zero-size pages and short/long buffers (never crash on drivers).
    pub fn validate(&self) -> Result<(), ScanError> {
        if self.width_px == 0 || self.height_px == 0 {
            return Err(ScanError::Driver("backend delivered a zero-size page".to_owned()));
        }
        let w = usize::try_from(self.width_px).map_err(|_| ScanError::Driver("page too wide".to_owned()))?;
        let h = usize::try_from(self.height_px).map_err(|_| ScanError::Driver("page too tall".to_owned()))?;
        let want = w.checked_mul(h).and_then(|n| n.checked_mul(3)).ok_or_else(|| ScanError::Driver("page too large".to_owned()))?;
        const CAP: usize = 512 * 1024 * 1024;
        if want > CAP || self.pixels_rgb8.len() != want {
            return Err(ScanError::Driver("page pixel buffer has the wrong length".to_owned()));
        }
        Ok(())
    }
}

/// Progress report streamed toward the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub pages_done: u32,
    pub stage: &'static str,
}

/// Cooperative cancellation checked between chunks/pages.
#[derive(Debug, Clone)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self { flag: Arc::new(AtomicBool::new(false)) }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<(), ScanError> {
        if self.is_cancelled() { Err(ScanError::Cancelled) } else { Ok(()) }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Sink receiving pages off the backend thread. Must be fast
/// (persist + notify) and never block the driver longer than needed.
pub trait PageSink {
    fn page(&mut self, page: RawPage) -> Result<(), ScanError>;
    fn progress(&mut self, progress: Progress);
}

/// Closed error enum with a human message each.
/// Mirrors NAPS2's `ScanDriverException` subtypes (offline/busy/feeder-empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    NoDevice,
    Busy,
    PaperEmpty,
    PaperJam,
    CoverOpen,
    Cancelled,
    Unsupported(String),
    Driver(String),
    Io(String),
}

impl ScanError {
    pub fn message(&self) -> String {
        match self {
            ScanError::NoDevice => "No scanner selected. Choose a device or scan profile first.".to_owned(),
            ScanError::Busy => "The scanner is busy. Wait for the current job to finish and try again.".to_owned(),
            ScanError::PaperEmpty => "The feeder is empty. Load paper and scan again.".to_owned(),
            ScanError::PaperJam => "Paper jam reported. Clear the feeder and try again.".to_owned(),
            ScanError::CoverOpen => "The scanner cover is open. Close it and try again.".to_owned(),
            ScanError::Cancelled => "Scan cancelled.".to_owned(),
            ScanError::Unsupported(what) => format!("Not supported by this device: {what}."),
            ScanError::Driver(msg) => format!("Scanner driver error: {msg}"),
            ScanError::Io(msg) => format!("I/O error during scan: {msg}"),
        }
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ScanError {}

/// Backend contract. All methods are synchronous and run off the UI thread;
/// slow calls (`list_devices`) must honour `cancel`.
pub trait ScannerBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn list_devices(&self, cancel: &CancelToken) -> Result<Vec<DeviceInfo>, ScanError>;
    fn capabilities(&self, device: &DeviceId) -> Result<Capabilities, ScanError>;
    fn scan(&self, request: &ScanRequest, sink: &mut dyn PageSink, cancel: &CancelToken) -> Result<u32, ScanError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_rejects_profile_without_device() {
        let mut profile = ScanProfile::default();
        assert_eq!(ScanRequest::from_profile(&profile), Err(ScanError::NoDevice));
        profile.device_id = "mock:demo".to_owned();
        assert!(ScanRequest::from_profile(&profile).is_ok());
    }

    #[test]
    fn cancel_token_roundtrip() {
        let t = CancelToken::new();
        assert!(t.check().is_ok());
        t.cancel();
        assert_eq!(t.check(), Err(ScanError::Cancelled));
    }

    #[test]
    fn raw_page_length_checked() {
        let bad = RawPage { width_px: 10, height_px: 10, dpi: 100, bit_depth: BitDepth::Color, side: PageSide::Front, pixels_rgb8: vec![0u8; 7] };
        assert!(bad.validate().is_err());
    }
}
