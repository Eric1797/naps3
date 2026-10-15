//! Deterministic fake scanner for tests and dev. No hardware, no OS calls.
//!
//! Two devices: `mock:flatbed` (1 page) and `mock:feeder` (3 pages).
//! Pages are vertical gradients derived from the page index so snapshots
//! are stable across runs.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use scancraft_model::{BitDepth, PageSide, PageSize, PaperSource};
use scancraft_scan_core::{CancelToken, Capabilities, DeviceId, DeviceInfo, PageSink, Progress, RawPage, ScanError, ScanRequest, ScannerBackend};

/// Fake backend id used in [`DeviceId`]s (`"mock:flatbed"`, `"mock:feeder"`).
pub const MOCK_BACKEND_ID: &str = "mock";

pub struct MockScanner;

impl MockScanner {
    pub fn new() -> Self {
        Self
    }

    fn pages_for(device: &DeviceId) -> Result<u32, ScanError> {
        match device.0.as_str() {
            "mock:flatbed" => Ok(1),
            "mock:feeder" => Ok(3),
            _ => Err(ScanError::NoDevice),
        }
    }

    /// Deterministic gradient page: brightness rises top-to-bottom,
    /// shifted per page index so multi-page runs are distinguishable.
    pub fn render_page(width: u32, height: u32, dpi: u32, page_index: u32) -> RawPage {
        let w = width.max(1) as usize;
        let h = height.max(1) as usize;
        let mut pixels = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            let v = (((y * 255) / h.max(1) + (page_index as usize * 40)) % 256) as u8;
            for x in 0..w {
                let u = ((x * 64) / w.max(1)) as u8;
                pixels.push(v);
                pixels.push(u);
                pixels.push(255u8.saturating_sub(v / 2));
            }
        }
        RawPage { width_px: width, height_px: height, dpi, bit_depth: BitDepth::Color, side: PageSide::Front, pixels_rgb8: pixels }
    }
}

impl Default for MockScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl ScannerBackend for MockScanner {
    fn id(&self) -> &'static str {
        MOCK_BACKEND_ID
    }

    fn list_devices(&self, cancel: &CancelToken) -> Result<Vec<DeviceInfo>, ScanError> {
        cancel.check()?;
        Ok(vec![
            DeviceInfo { id: DeviceId("mock:flatbed".to_owned()), backend: MOCK_BACKEND_ID, display_name: "Mock Flatbed".to_owned() },
            DeviceInfo { id: DeviceId("mock:feeder".to_owned()), backend: MOCK_BACKEND_ID, display_name: "Mock Feeder (3 pages)".to_owned() },
        ])
    }

    fn capabilities(&self, device: &DeviceId) -> Result<Capabilities, ScanError> {
        Self::pages_for(device)?;
        Ok(Capabilities {
            sources: vec![PaperSource::Flatbed, PaperSource::Feeder],
            dpis: vec![100, 200, 300],
            bit_depths: vec![BitDepth::Color, BitDepth::Grayscale, BitDepth::BlackWhite],
            page_sizes: vec![PageSize::Letter, PageSize::A4],
            duplex: false,
        })
    }

    fn scan(&self, request: &ScanRequest, sink: &mut dyn PageSink, cancel: &CancelToken) -> Result<u32, ScanError> {
        let total = Self::pages_for(&request.device)?;
        let (w, h) = request.page_size.dims_px(request.dpi).map_err(|e| ScanError::Unsupported(e.to_string()))?;
        // Keep mock pages small so tests stay fast: scale to ~200px wide.
        let scale = 200.0 / f64::from(w.max(1));
        let mw = ((f64::from(w) * scale).round() as u32).clamp(32, 400);
        let mh = ((f64::from(h) * scale).round() as u32).clamp(32, 600);
        for i in 0..total {
            cancel.check()?;
            let page = Self::render_page(mw, mh, request.dpi, i);
            page.validate()?;
            sink.page(page)?;
            sink.progress(Progress { pages_done: i + 1, stage: "scan" });
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Collect {
        pages: Vec<RawPage>,
    }

    impl PageSink for Collect {
        fn page(&mut self, page: RawPage) -> Result<(), ScanError> {
            self.pages.push(page);
            Ok(())
        }

        fn progress(&mut self, _progress: Progress) {}
    }

    #[test]
    fn lists_two_devices() {
        let m = MockScanner::new();
        let devs = m.list_devices(&CancelToken::new()).unwrap();
        assert_eq!(devs.len(), 2);
    }

    #[test]
    fn feeder_delivers_three_pages() {
        let m = MockScanner::new();
        let req = ScanRequest {
            device: DeviceId("mock:feeder".to_owned()),
            paper_source: PaperSource::Feeder,
            page_size: PageSize::Letter,
            dpi: 200,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        };
        let mut sink = Collect { pages: Vec::new() };
        let n = m.scan(&req, &mut sink, &CancelToken::new()).unwrap();
        assert_eq!(n, 3);
        assert_eq!(sink.pages.len(), 3);
    }

    #[test]
    fn cancel_stops_scan() {
        let m = MockScanner::new();
        let req = ScanRequest {
            device: DeviceId("mock:feeder".to_owned()),
            paper_source: PaperSource::Feeder,
            page_size: PageSize::Letter,
            dpi: 200,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        };
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut sink = Collect { pages: Vec::new() };
        assert_eq!(m.scan(&req, &mut sink, &cancel), Err(ScanError::Cancelled));
    }
}
