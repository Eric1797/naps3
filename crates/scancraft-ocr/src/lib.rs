//! OCR engine trait plus a Tesseract subprocess engine for ScanCraft.
//!
//! Ported from NAPS2 (GPL-2.0-or-later): mirrors the `IOcrEngine` /
//! `TesseractOcrEngine` vocabulary from `NAPS2.Ocr`. Like NAPS2, we shell out
//! to a `tesseract` binary instead of linking OCR code: the page is encoded
//! to PNG, written to a temp file, and `tesseract <img> stdout -l <lang>` is
//! run with its stdout parsed into words.
//!
//! No network access happens here; the only subprocess used is the local
//! Tesseract binary via [`std::process::Command`].

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use scancraft_imaging::encode_png;
use scancraft_scan_core::{RawPage, ScanError};

/// Maximum stdout words kept from one Tesseract run (bounds memory on noise).
const MAX_WORDS: usize = 100_000;

/// Maximum characters kept from Tesseract's stderr when reporting a failure.
const MAX_STDERR_CHARS: usize = 500;

/// OCR request options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrParams {
    /// Tesseract language code, e.g. `"eng"`.
    pub language: String,
}

impl Default for OcrParams {
    fn default() -> Self {
        Self { language: "eng".to_owned() }
    }
}

impl OcrParams {
    /// Build params for the given Tesseract language code.
    pub fn new(language: &str) -> Self {
        Self { language: language.to_owned() }
    }
}

/// One recognized word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrWord {
    pub text: String,
}

/// Words recognized on a page, in reading order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OcrResult {
    pub words: Vec<OcrWord>,
}

impl OcrResult {
    /// Space-joined word texts.
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }
}

/// OCR engine contract: recognize the words on a page.
pub trait OcrEngine {
    fn recognize(&self, page: &RawPage, params: &OcrParams) -> Result<OcrResult, ScanError>;
    fn name(&self) -> &'static str;
}

/// No-op engine for tests and development without Tesseract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StubOcrEngine;

impl OcrEngine for StubOcrEngine {
    fn recognize(&self, _page: &RawPage, _params: &OcrParams) -> Result<OcrResult, ScanError> {
        Ok(OcrResult { words: Vec::new() })
    }

    fn name(&self) -> &'static str {
        "stub"
    }
}

/// OCR engine that shells out to a local `tesseract` binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TesseractOcrEngine {
    program: String,
}

impl Default for TesseractOcrEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TesseractOcrEngine {
    /// Use `tesseract` resolved from `PATH`.
    pub fn new() -> Self {
        Self { program: "tesseract".to_owned() }
    }

    /// Use an explicit binary path (useful for tests and custom installs).
    pub fn with_program(program: &str) -> Self {
        Self { program: program.to_owned() }
    }
}

impl OcrEngine for TesseractOcrEngine {
    fn name(&self) -> &'static str {
        "tesseract"
    }

    fn recognize(&self, page: &RawPage, params: &OcrParams) -> Result<OcrResult, ScanError> {
        if params.language.trim().is_empty() {
            return Err(ScanError::Unsupported("ocr language must be set".to_owned()));
        }
        let png = encode_png(page)?;

        // Process-unique temp name so parallel scans never collide.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let mut path = std::env::temp_dir();
        path.push(format!("scancraft-ocr-{pid}-{id}.png"));

        if let Err(e) = std::fs::write(&path, &png) {
            return Err(ScanError::Io(format!("could not write ocr temp image: {e}")));
        }

        let mut command = Command::new(&self.program);
        command.arg(&path).arg("stdout").arg("-l").arg(&params.language);
        #[cfg(target_os = "windows")]
        {
            // Keep the helper console from flashing over the GUI.
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        let output = match command.output() {
            Ok(output) => output,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Err(ScanError::Unsupported("Tesseract is not installed or not on PATH; install Tesseract to enable OCR".to_owned()));
                }
                return Err(ScanError::Io(format!("could not launch tesseract: {e}")));
            }
        };
        // Best-effort cleanup; the recognition result matters, not the unlink.
        let _ = std::fs::remove_file(&path);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let capped: String = stderr.trim().chars().take(MAX_STDERR_CHARS).collect();
            let status = output.status;
            return Err(ScanError::Driver(format!("tesseract failed ({status}): {capped}")));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut words: Vec<OcrWord> = Vec::new();
        for token in stdout.split_whitespace().take(MAX_WORDS) {
            words.push(OcrWord { text: token.to_owned() });
        }
        Ok(OcrResult { words })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use scancraft_model::{BitDepth, PageSide};

    fn test_page() -> RawPage {
        RawPage { width_px: 1, height_px: 1, dpi: 200, bit_depth: BitDepth::Color, side: PageSide::Front, pixels_rgb8: vec![255u8; 3] }
    }

    #[test]
    fn params_default_language_is_eng() {
        assert_eq!(OcrParams::default().language, "eng");
        assert_eq!(OcrParams::new("deu").language, "deu");
    }

    #[test]
    fn result_text_joins_words_with_spaces() {
        let result = OcrResult { words: vec![OcrWord { text: "hello".to_owned() }, OcrWord { text: "world".to_owned() }] };
        assert_eq!(result.text(), "hello world");
        assert_eq!(OcrResult::default().text(), "");
    }

    #[test]
    fn stub_returns_empty_result() {
        let engine = StubOcrEngine;
        assert_eq!(engine.name(), "stub");
        let result = engine.recognize(&test_page(), &OcrParams::default()).unwrap();
        assert!(result.words.is_empty());
        assert_eq!(result.text(), "");
    }

    #[test]
    fn missing_binary_maps_to_unsupported() {
        let engine = TesseractOcrEngine::with_program("definitely-not-a-real-binary-xyz");
        assert_eq!(engine.name(), "tesseract");
        let err = engine.recognize(&test_page(), &OcrParams::default()).unwrap_err();
        assert!(matches!(err, ScanError::Unsupported(_)));
    }

    #[test]
    fn empty_language_is_rejected_without_spawning() {
        let engine = TesseractOcrEngine::new();
        let result = engine.recognize(&test_page(), &OcrParams::new(""));
        assert!(result.is_err());
    }
}
