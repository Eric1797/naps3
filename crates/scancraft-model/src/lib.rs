//! Pure data model for ScanCraft. No I/O, no OS calls, no UI.
//!
//! Vocabulary mirrors NAPS2 (`NAPS2.Sdk/Scan/*`, `NAPS2.Lib/Scan/ScanProfile.cs`
//! in `naps2-in c++-master #For Reference/`) so profiles and docs translate
//! 1:1. All input-derived values validate via `Result`. Logic is ported,
//! never bulk-copied; this project is GPL-2.0-or-later like NAPS2.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::fmt;

/// Driver used for the actual scan. Availability depends on the platform.
///
/// Mirrors `NAPS2.Sdk/Scan/Driver.cs`: Windows defaults to WIA,
/// macOS to Apple, Linux to SANE; eSCL works everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Driver {
    /// Platform default (WIA on Windows, Apple on macOS, SANE on Linux).
    #[default]
    Default,
    Wia,
    Twain,
    Apple,
    Sane,
    Escl,
}

impl Driver {
    /// Resolve `Default` to the platform driver, pass the rest through.
    pub fn resolve(self) -> Self {
        match self {
            Driver::Default => Self::system_default(),
            other => other,
        }
    }

    /// Platform default driver.
    pub fn system_default() -> Self {
        if cfg!(target_os = "windows") {
            Driver::Wia
        } else if cfg!(target_os = "macos") {
            Driver::Apple
        } else {
            Driver::Sane
        }
    }

    /// Parse the `DriverName` string stored on a NAPS2-style profile
    /// (`"wia"`, `"twain"`, `"sane"`, `"escl"`, `"apple"`).
    pub fn parse_name(name: &str) -> Self {
        if name.eq_ignore_ascii_case("wia") {
            Driver::Wia
        } else if name.eq_ignore_ascii_case("twain") {
            Driver::Twain
        } else if name.eq_ignore_ascii_case("apple") {
            Driver::Apple
        } else if name.eq_ignore_ascii_case("sane") {
            Driver::Sane
        } else if name.eq_ignore_ascii_case("escl") {
            Driver::Escl
        } else {
            Driver::Default
        }
    }
}

impl fmt::Display for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Driver::Default => "default",
            Driver::Wia => "wia",
            Driver::Twain => "twain",
            Driver::Apple => "apple",
            Driver::Sane => "sane",
            Driver::Escl => "escl",
        };
        f.write_str(s)
    }
}

/// Physical paper source. Mirrors `NAPS2.Sdk/Scan/PaperSource.cs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PaperSource {
    Auto,
    #[default]
    Flatbed,
    Feeder,
    Duplex,
}

/// Profile-level source names. Mirrors `ScanSource` in `ScanProfile.cs`
/// (`Glass`/`Feeder`/`Duplex`) and maps onto [`PaperSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ScanSource {
    #[default]
    Glass,
    Feeder,
    Duplex,
}

impl ScanSource {
    pub fn to_paper_source(self) -> PaperSource {
        match self {
            ScanSource::Glass => PaperSource::Flatbed,
            ScanSource::Feeder => PaperSource::Feeder,
            ScanSource::Duplex => PaperSource::Duplex,
        }
    }
}

/// Bit depth of a scanned page. Mirrors `ScanBitDepth`/`BitDepth` in NAPS2
/// (`C24Bit` profile name, `Color` driver name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BitDepth {
    #[default]
    Color,
    Grayscale,
    BlackWhite,
}

/// Front or back side of a duplex sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PageSide {
    #[default]
    Front,
    Back,
}

/// Horizontal placement of the page inside a larger scan area.
/// NAPS2 default is `Right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HorizontalAlign {
    Left,
    Center,
    #[default]
    Right,
}

/// Standard page sizes plus custom (millimetres).
/// Dimensions mirror the `PageDimensions` attributes on `ScanPageSize`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PageSize {
    A4,
    #[default]
    Letter,
    Legal,
    A5,
    A3,
    B4,
    B5,
    Custom {
        width_mm: f64,
        height_mm: f64,
    },
}

impl PageSize {
    /// Dimensions in millimetres. Errors on non-positive or absurd customs.
    pub fn dims_mm(self) -> Result<(f64, f64), ModelError> {
        match self {
            PageSize::A4 => Ok((210.0, 297.0)),
            PageSize::Letter => Ok((215.9, 279.4)),
            PageSize::Legal => Ok((215.9, 355.6)),
            PageSize::A5 => Ok((148.0, 210.0)),
            PageSize::A3 => Ok((297.0, 420.0)),
            PageSize::B4 => Ok((250.0, 353.0)),
            PageSize::B5 => Ok((176.0, 250.0)),
            PageSize::Custom { width_mm, height_mm } => {
                if !width_mm.is_finite() || !height_mm.is_finite() || width_mm <= 0.0 || height_mm <= 0.0 {
                    return Err(ModelError::InvalidPageSize);
                }
                if width_mm > 2000.0 || height_mm > 2000.0 {
                    return Err(ModelError::InvalidPageSize);
                }
                Ok((width_mm, height_mm))
            }
        }
    }

    /// Pixel extents of the page at `dpi`, or an error when out of range.
    pub fn dims_px(self, dpi: u32) -> Result<(u32, u32), ModelError> {
        if !(50..=4800).contains(&dpi) {
            return Err(ModelError::InvalidDpi);
        }
        let (w_mm, h_mm) = self.dims_mm()?;
        let w = w_mm * f64::from(dpi) / 25.4;
        let h = h_mm * f64::from(dpi) / 25.4;
        if !w.is_finite() || !h.is_finite() || w < 1.0 || h < 1.0 || w > 100_000.0 || h > 100_000.0 {
            return Err(ModelError::InvalidPageSize);
        }
        Ok((w.round() as u32, h.round() as u32))
    }
}

/// A physical scanner. Mirrors `NAPS2.Sdk/Scan/ScanDevice.cs`.
///
/// `id` semantics depend on the driver: WIA device id, TWAIN identity,
/// SANE `backend:name`, eSCL UUID or `http(s)://` URI, Apple UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanDevice {
    pub driver: Driver,
    pub id: String,
    pub name: String,
    pub connection_uri: Option<String>,
}

impl ScanDevice {
    pub fn new(driver: Driver, id: &str, name: &str) -> Self {
        Self { driver, id: id.to_owned(), name: name.to_owned(), connection_uri: None }
    }
}

/// User-persisted scan configuration. Mirrors `NAPS2.Lib/Scan/ScanProfile.cs`
/// defaults: 24-bit color, right align, Letter, 200 DPI, flatbed glass,
/// JPEG quality 75, blank thresholds 70/15.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanProfile {
    pub display_name: String,
    pub driver: Driver,
    pub device_id: String,
    pub paper_source: ScanSource,
    pub dpi: u32,
    pub page_size: PageSize,
    pub bit_depth: BitDepth,
    pub page_align: HorizontalAlign,
    pub brightness: i32,
    pub contrast: i32,
    pub quality: i32,
    pub max_quality: bool,
    pub exclude_blank_pages: bool,
    pub blank_white_threshold: i32,
    pub blank_coverage_threshold: i32,
    pub auto_deskew: bool,
    pub auto_crop: bool,
    pub rotate_degrees: f64,
    pub use_native_ui: bool,
}

impl Default for ScanProfile {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            driver: Driver::Default,
            device_id: String::new(),
            paper_source: ScanSource::Glass,
            dpi: 200,
            page_size: PageSize::Letter,
            bit_depth: BitDepth::Color,
            page_align: HorizontalAlign::Right,
            brightness: 0,
            contrast: 0,
            quality: 75,
            max_quality: false,
            exclude_blank_pages: false,
            blank_white_threshold: 70,
            blank_coverage_threshold: 15,
            auto_deskew: false,
            auto_crop: false,
            rotate_degrees: 0.0,
            use_native_ui: false,
        }
    }
}

impl ScanProfile {
    /// Validate user-editable values before a scan starts.
    pub fn validate(&self) -> Result<(), ModelError> {
        if !(50..=4800).contains(&self.dpi) {
            return Err(ModelError::InvalidDpi);
        }
        if self.brightness < -1000 || self.brightness > 1000 || self.contrast < -1000 || self.contrast > 1000 {
            return Err(ModelError::InvalidBrightnessContrast);
        }
        if self.quality < 0 || self.quality > 100 {
            return Err(ModelError::InvalidQuality);
        }
        if !self.rotate_degrees.is_finite() {
            return Err(ModelError::InvalidRotation);
        }
        self.page_size.dims_mm().map(|_| ())
    }
}

/// Per-scan driver options. Mirrors `NAPS2.Sdk/Scan/ScanOptions.cs`
/// (JPEG quality 75, blank thresholds 70/15, scale ratio 1).
#[derive(Debug, Clone, PartialEq)]
pub struct ScanOptions {
    pub driver: Driver,
    pub device: Option<ScanDevice>,
    pub paper_source: PaperSource,
    pub dpi: u32,
    pub scale_ratio: u32,
    pub page_size: PageSize,
    pub brightness: i32,
    pub contrast: i32,
    pub bit_depth: BitDepth,
    pub page_align: HorizontalAlign,
    pub use_native_ui: bool,
    pub exclude_blank_pages: bool,
    pub blank_white_threshold: i32,
    pub blank_coverage_threshold: i32,
    pub max_quality: bool,
    pub quality: i32,
    pub auto_deskew: bool,
    pub auto_crop: bool,
    pub rotate_degrees: f64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            driver: Driver::Default,
            device: None,
            paper_source: PaperSource::Flatbed,
            dpi: 200,
            scale_ratio: 1,
            page_size: PageSize::Letter,
            brightness: 0,
            contrast: 0,
            bit_depth: BitDepth::Color,
            page_align: HorizontalAlign::Right,
            use_native_ui: false,
            exclude_blank_pages: false,
            blank_white_threshold: 70,
            blank_coverage_threshold: 15,
            max_quality: false,
            quality: 75,
            auto_deskew: false,
            auto_crop: false,
            rotate_degrees: 0.0,
        }
    }
}

impl ScanOptions {
    /// Build from a user profile (the `ScanPerformer.BuildOptions` step).
    pub fn from_profile(profile: &ScanProfile, device: Option<ScanDevice>) -> Result<Self, ModelError> {
        profile.validate()?;
        Ok(Self {
            driver: profile.driver.resolve(),
            device,
            paper_source: profile.paper_source.to_paper_source(),
            dpi: profile.dpi,
            scale_ratio: 1,
            page_size: profile.page_size,
            brightness: profile.brightness,
            contrast: profile.contrast,
            bit_depth: profile.bit_depth,
            page_align: profile.page_align,
            use_native_ui: profile.use_native_ui,
            exclude_blank_pages: profile.exclude_blank_pages,
            blank_white_threshold: profile.blank_white_threshold,
            blank_coverage_threshold: profile.blank_coverage_threshold,
            max_quality: profile.max_quality,
            quality: profile.quality,
            auto_deskew: profile.auto_deskew,
            auto_crop: profile.auto_crop,
            rotate_degrees: profile.rotate_degrees,
        })
    }
}

/// Closed validation errors for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelError {
    InvalidDpi,
    InvalidPageSize,
    InvalidBrightnessContrast,
    InvalidQuality,
    InvalidRotation,
}

impl ModelError {
    pub fn message(self) -> &'static str {
        match self {
            ModelError::InvalidDpi => "DPI must be between 50 and 4800.",
            ModelError::InvalidPageSize => "Page size is invalid.",
            ModelError::InvalidBrightnessContrast => "Brightness and contrast must be between -1000 and 1000.",
            ModelError::InvalidQuality => "JPEG quality must be between 0 and 100.",
            ModelError::InvalidRotation => "Rotation must be a finite number of degrees.",
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ModelError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_defaults_match_naps2() {
        let p = ScanProfile::default();
        assert_eq!(p.dpi, 200);
        assert_eq!(p.quality, 75);
        assert_eq!(p.blank_white_threshold, 70);
        assert_eq!(p.blank_coverage_threshold, 15);
        assert_eq!(p.bit_depth, BitDepth::Color);
        assert_eq!(p.page_align, HorizontalAlign::Right);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn page_dims_letter_at_200dpi() {
        let (w, h) = PageSize::Letter.dims_px(200).unwrap();
        assert!(w > 1600 && w < 1750);
        assert!(h > 2100 && h < 2250);
    }

    #[test]
    fn rejects_bad_dpi() {
        let p = ScanProfile { dpi: 10, ..ScanProfile::default() };
        assert_eq!(p.validate(), Err(ModelError::InvalidDpi));
    }

    #[test]
    fn driver_name_parsing() {
        assert_eq!(Driver::parse_name("wia"), Driver::Wia);
        assert_eq!(Driver::parse_name("ESCL"), Driver::Escl);
        assert_eq!(Driver::parse_name("nope"), Driver::Default);
    }
}
