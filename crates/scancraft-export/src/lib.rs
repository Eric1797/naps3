//! PDF/TIFF/BMP/JPEG/PNG exporters for ScanCraft (pure Rust).
//!
//! Ported from NAPS2 (GPL-2.0-or-later): mirrors the `SavePdfOperation` and
//! `SaveImagesOperation` concepts from `NAPS2.ImportExport` — take scanned
//! pages plus save options and produce a finished file in memory.
//!
//! The PDF and TIFF writers here are intentionally minimal and dependency-free:
//! pages are embedded as RAW (unfiltered) samples. Flate compression and
//! JPEG passthrough are a later milestone; files produced today are large
//! but valid.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use scancraft_imaging::{encode_bmp, encode_jpeg, encode_png};
use scancraft_scan_core::{RawPage, ScanError};

/// Output format chosen for [`export_pages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Pdf,
    Tiff,
    Bmp,
    Jpeg,
    Png,
}

/// Map a file extension (with or without a leading dot, any case) to an
/// [`ExportFormat`]. Returns `None` for unknown extensions.
pub fn format_from_extension(ext: &str) -> Option<ExportFormat> {
    let trimmed = ext.trim();
    let without_dot = trimmed.strip_prefix('.').unwrap_or(trimmed);
    if without_dot.eq_ignore_ascii_case("pdf") {
        Some(ExportFormat::Pdf)
    } else if without_dot.eq_ignore_ascii_case("tif") || without_dot.eq_ignore_ascii_case("tiff") {
        Some(ExportFormat::Tiff)
    } else if without_dot.eq_ignore_ascii_case("bmp") {
        Some(ExportFormat::Bmp)
    } else if without_dot.eq_ignore_ascii_case("jpg") || without_dot.eq_ignore_ascii_case("jpeg") {
        Some(ExportFormat::Jpeg)
    } else if without_dot.eq_ignore_ascii_case("png") {
        Some(ExportFormat::Png)
    } else {
        None
    }
}

/// Document metadata written into the PDF Info dictionary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdfMetadata {
    pub title: String,
    pub author: String,
}

/// Largest page dimension (in pixels) accepted for export.
const MAX_DIM_PX: u32 = 20_000;

/// Reject degenerate or inconsistent pages before any writer touches them.
fn validate_page(page: &RawPage) -> Result<(), ScanError> {
    if page.width_px == 0 || page.height_px == 0 {
        return Err(ScanError::Driver("cannot export a zero-size page".to_owned()));
    }
    if page.dpi == 0 {
        return Err(ScanError::Driver("cannot export a page with zero dpi".to_owned()));
    }
    if page.width_px > MAX_DIM_PX || page.height_px > MAX_DIM_PX {
        return Err(ScanError::Driver("page dimensions exceed the export limit".to_owned()));
    }
    let w = usize::try_from(page.width_px).map_err(|_| ScanError::Driver("page too wide to export".to_owned()))?;
    let h = usize::try_from(page.height_px).map_err(|_| ScanError::Driver("page too tall to export".to_owned()))?;
    let want = w.checked_mul(h).and_then(|n| n.checked_mul(3)).ok_or_else(|| ScanError::Driver("page too large to export".to_owned()))?;
    if page.pixels_rgb8.len() != want {
        return Err(ScanError::Driver("page pixel buffer has the wrong length".to_owned()));
    }
    Ok(())
}

/// Export pages to the requested format.
///
/// * PDF and TIFF accept any non-empty page slice (multi-page output).
/// * BMP, PNG and JPEG export exactly one page; anything else is
///   [`ScanError::Unsupported`].
/// * `jpeg_quality` (0-100) is forwarded to the JPEG encoder.
pub fn export_pages(pages: &[RawPage], format: ExportFormat, meta: &PdfMetadata, jpeg_quality: u8) -> Result<Vec<u8>, ScanError> {
    if pages.is_empty() {
        return Err(ScanError::Unsupported("no pages to export".to_owned()));
    }
    for page in pages {
        validate_page(page)?;
    }
    match format {
        ExportFormat::Bmp | ExportFormat::Png | ExportFormat::Jpeg => {
            if pages.len() != 1 {
                return Err(ScanError::Unsupported("this image format exports exactly one page".to_owned()));
            }
            let page = pages.first().ok_or_else(|| ScanError::Unsupported("no pages to export".to_owned()))?;
            match format {
                ExportFormat::Bmp => encode_bmp(page),
                ExportFormat::Png => encode_png(page),
                ExportFormat::Jpeg => encode_jpeg(page, jpeg_quality),
                ExportFormat::Pdf | ExportFormat::Tiff => Err(ScanError::Unsupported("unreachable export branch".to_owned())),
            }
        }
        ExportFormat::Tiff => write_tiff(pages),
        ExportFormat::Pdf => write_pdf(pages, meta),
    }
}

// ---------------------------------------------------------------------------
// Minimal uncompressed RGB TIFF writer (classic TIFF, little-endian).
// ---------------------------------------------------------------------------

fn push_entry(out: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&typ.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_tiff(pages: &[RawPage]) -> Result<Vec<u8>, ScanError> {
    const TYPE_SHORT: u16 = 3;
    const TYPE_LONG: u16 = 4;
    const TYPE_RATIONAL: u16 = 5;
    const TAG_WIDTH: u16 = 256;
    const TAG_HEIGHT: u16 = 257;
    const TAG_BITS_PER_SAMPLE: u16 = 258;
    const TAG_COMPRESSION: u16 = 259;
    const TAG_PHOTOMETRIC: u16 = 262;
    const TAG_STRIP_OFFSETS: u16 = 273;
    const TAG_SAMPLES_PER_PIXEL: u16 = 277;
    const TAG_ROWS_PER_STRIP: u16 = 278;
    const TAG_STRIP_BYTE_COUNTS: u16 = 279;
    const TAG_X_RESOLUTION: u16 = 282;
    const TAG_Y_RESOLUTION: u16 = 283;
    const TAG_RESOLUTION_UNIT: u16 = 296;
    const ENTRIES_PER_IFD: u64 = 12;
    // 2 bytes entry count + 12 bytes per entry + 4 bytes next-IFD offset.
    const IFD_LEN: u64 = 2 + 12 * ENTRIES_PER_IFD + 4;
    // Per page: BitsPerSample (3 x u16) + XResolution (2 x u32) + YResolution.
    const EXTRA_PER_PAGE: u64 = 6 + 8 + 8;
    // Entry count written into every IFD header below.
    const ENTRIES_PER_IFD_U16: u16 = 12;

    let overflow = || ScanError::Io("tiff export overflowed file offsets".to_owned());

    // Byte length of each page's raw strip.
    let mut strip_lens: Vec<u64> = Vec::new();
    for page in pages {
        let w = u64::from(page.width_px);
        let h = u64::from(page.height_px);
        let len = w.checked_mul(h).and_then(|n| n.checked_mul(3)).ok_or_else(overflow)?;
        strip_lens.push(len);
    }

    // Lay out: header (8 bytes), all strips, all per-page extras, all IFDs.
    let mut strip_offsets: Vec<u64> = Vec::new();
    let mut cursor: u64 = 8;
    for len in &strip_lens {
        strip_offsets.push(cursor);
        cursor = cursor.checked_add(*len).ok_or_else(overflow)?;
    }
    let mut extra_offsets: Vec<u64> = Vec::new();
    for _ in pages {
        extra_offsets.push(cursor);
        cursor = cursor.checked_add(EXTRA_PER_PAGE).ok_or_else(overflow)?;
    }
    let mut ifd_offsets: Vec<u64> = Vec::new();
    for _ in pages {
        ifd_offsets.push(cursor);
        cursor = cursor.checked_add(IFD_LEN).ok_or_else(overflow)?;
    }
    let total_len = cursor;

    // Classic TIFF offsets are 32-bit; fail cleanly instead of truncating.
    let first_ifd_u32 = ifd_offsets.first().copied().ok_or_else(overflow).and_then(|o| u32::try_from(o).map_err(|_| overflow()))?;
    let mut strip_offsets_u32: Vec<u32> = Vec::new();
    for o in &strip_offsets {
        strip_offsets_u32.push(u32::try_from(*o).map_err(|_| overflow())?);
    }
    let mut strip_lens_u32: Vec<u32> = Vec::new();
    for l in &strip_lens {
        strip_lens_u32.push(u32::try_from(*l).map_err(|_| overflow())?);
    }
    let mut extra_offsets_u32: Vec<u32> = Vec::new();
    for o in &extra_offsets {
        extra_offsets_u32.push(u32::try_from(*o).map_err(|_| overflow())?);
    }
    let mut ifd_offsets_u32: Vec<u32> = Vec::new();
    for o in &ifd_offsets {
        ifd_offsets_u32.push(u32::try_from(*o).map_err(|_| overflow())?);
    }
    let total_usize = usize::try_from(total_len).map_err(|_| overflow())?;

    let mut out: Vec<u8> = Vec::new();
    if out.try_reserve(total_usize).is_err() {
        return Err(ScanError::Io("tiff export output is too large to allocate".to_owned()));
    }

    // Header: little-endian magic + first IFD offset.
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&first_ifd_u32.to_le_bytes());

    // Strips (raw RGB rows, top to bottom).
    for page in pages {
        out.extend_from_slice(&page.pixels_rgb8);
    }

    // Per-page extras: BitsPerSample 8,8,8 + X/YResolution dpi/1.
    for page in pages {
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&page.dpi.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&page.dpi.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
    }

    // One IFD per page.
    for (i, page) in pages.iter().enumerate() {
        let index = usize::try_from(u64::try_from(i).map_err(|_| overflow())?).map_err(|_| overflow())?;
        let strip_off = strip_offsets_u32.get(index).copied().ok_or_else(overflow)?;
        let strip_len = strip_lens_u32.get(index).copied().ok_or_else(overflow)?;
        let extra_off = extra_offsets_u32.get(index).copied().ok_or_else(overflow)?;
        let bits_off = extra_off;
        let xres_off = extra_off.checked_add(6).ok_or_else(overflow)?;
        let yres_off = extra_off.checked_add(14).ok_or_else(overflow)?;
        let next_ifd = if index.checked_add(1).is_some_and(|next| next < ifd_offsets_u32.len()) {
            ifd_offsets_u32.get(index.saturating_add(1)).copied().ok_or_else(overflow)?
        } else {
            0
        };

        out.extend_from_slice(&ENTRIES_PER_IFD_U16.to_le_bytes());
        push_entry(&mut out, TAG_WIDTH, TYPE_LONG, 1, page.width_px);
        push_entry(&mut out, TAG_HEIGHT, TYPE_LONG, 1, page.height_px);
        push_entry(&mut out, TAG_BITS_PER_SAMPLE, TYPE_SHORT, 3, bits_off);
        push_entry(&mut out, TAG_COMPRESSION, TYPE_SHORT, 1, 1);
        push_entry(&mut out, TAG_PHOTOMETRIC, TYPE_SHORT, 1, 2);
        push_entry(&mut out, TAG_STRIP_OFFSETS, TYPE_LONG, 1, strip_off);
        push_entry(&mut out, TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, 3);
        push_entry(&mut out, TAG_ROWS_PER_STRIP, TYPE_LONG, 1, page.height_px);
        push_entry(&mut out, TAG_STRIP_BYTE_COUNTS, TYPE_LONG, 1, strip_len);
        push_entry(&mut out, TAG_X_RESOLUTION, TYPE_RATIONAL, 1, xres_off);
        push_entry(&mut out, TAG_Y_RESOLUTION, TYPE_RATIONAL, 1, yres_off);
        push_entry(&mut out, TAG_RESOLUTION_UNIT, TYPE_SHORT, 1, 2);
        out.extend_from_slice(&next_ifd.to_le_bytes());
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Minimal PDF 1.4 writer: one DeviceRGB image XObject per page with RAW
// (unfiltered) samples. Files are large; Flate compression and JPEG
// passthrough are a later milestone.
// ---------------------------------------------------------------------------

/// Append PDF literal-string bytes for `s`, escaping `\`, `(`, `)` and
/// newlines.
fn pdf_push_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'(');
    for b in s.bytes() {
        match b {
            b'\\' | b'(' | b')' => {
                out.push(b'\\');
                out.push(b);
            }
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\n' => out.extend_from_slice(b"\\n"),
            other => out.push(other),
        }
    }
    out.push(b')');
}

fn pdf_begin_obj(out: &mut Vec<u8>, offsets: &mut Vec<u64>, num: u64) -> Result<(), ScanError> {
    let off = u64::try_from(out.len()).map_err(|_| ScanError::Io("pdf export overflowed file offsets".to_owned()))?;
    offsets.push(off);
    out.extend_from_slice(num.to_string().as_bytes());
    out.extend_from_slice(b" 0 obj\n");
    Ok(())
}

fn write_pdf(pages: &[RawPage], meta: &PdfMetadata) -> Result<Vec<u8>, ScanError> {
    let overflow = || ScanError::Io("pdf export overflowed file offsets".to_owned());
    let page_total = u64::try_from(pages.len()).map_err(|_| overflow())?;

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n");
    out.extend_from_slice("%\u{e2}\u{e3}\u{cf}\u{d3}\n".as_bytes());

    // Object 0 is the free head; offsets[i] holds the file offset of object i.
    let mut offsets: Vec<u64> = vec![0];

    // Per-page object numbers: page = 3 + 3i, content = 4 + 3i, image = 5 + 3i.
    // Info dict follows the last page triple.
    let info_num = 3u64.checked_add(page_total.checked_mul(3).ok_or_else(overflow)?).ok_or_else(overflow)?;

    // Catalog.
    pdf_begin_obj(&mut out, &mut offsets, 1)?;
    out.extend_from_slice(b"<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    // Pages collection.
    pdf_begin_obj(&mut out, &mut offsets, 2)?;
    out.extend_from_slice(b"<< /Type /Pages /Kids [");
    for i in 0..pages.len() {
        let page_num = 3u64.checked_add(u64::try_from(i).map_err(|_| overflow())?.checked_mul(3).ok_or_else(overflow)?).ok_or_else(overflow)?;
        out.extend_from_slice(page_num.to_string().as_bytes());
        out.extend_from_slice(b" 0 R ");
    }
    out.extend_from_slice(b"] /Count ");
    out.extend_from_slice(pages.len().to_string().as_bytes());
    out.extend_from_slice(b" >>\nendobj\n");

    // Page / content / image triples.
    for (i, page) in pages.iter().enumerate() {
        let base = 3u64.checked_add(u64::try_from(i).map_err(|_| overflow())?.checked_mul(3).ok_or_else(overflow)?).ok_or_else(overflow)?;
        let page_num = base;
        let content_num = base.checked_add(1).ok_or_else(overflow)?;
        let image_num = base.checked_add(2).ok_or_else(overflow)?;

        // Points (1/72 inch) from pixels at the page dpi.
        let w_pt = f64::from(page.width_px) * 72.0 / f64::from(page.dpi);
        let h_pt = f64::from(page.height_px) * 72.0 / f64::from(page.dpi);
        let w_txt = format!("{w_pt:.2}");
        let h_txt = format!("{h_pt:.2}");

        // Page object.
        pdf_begin_obj(&mut out, &mut offsets, page_num)?;
        out.extend_from_slice(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 ");
        out.extend_from_slice(w_txt.as_bytes());
        out.push(b' ');
        out.extend_from_slice(h_txt.as_bytes());
        out.extend_from_slice(b"] /Resources << /XObject << /Im0 ");
        out.extend_from_slice(image_num.to_string().as_bytes());
        out.extend_from_slice(b" 0 R >> >> /Contents ");
        out.extend_from_slice(content_num.to_string().as_bytes());
        out.extend_from_slice(b" 0 R >>\nendobj\n");

        // Content stream: draw the image over the full page.
        let mut content: Vec<u8> = Vec::new();
        content.extend_from_slice(b"q\n");
        content.extend_from_slice(w_txt.as_bytes());
        content.extend_from_slice(b" 0 0 ");
        content.extend_from_slice(h_txt.as_bytes());
        content.extend_from_slice(b" 0 0 cm\n/Im0 Do\nQ\n");
        let content_len = u64::try_from(content.len()).map_err(|_| overflow())?;
        pdf_begin_obj(&mut out, &mut offsets, content_num)?;
        out.extend_from_slice(b"<< /Length ");
        out.extend_from_slice(content_len.to_string().as_bytes());
        out.extend_from_slice(b" >>\nstream\n");
        out.extend_from_slice(&content);
        out.extend_from_slice(b"endstream\nendobj\n");

        // Image XObject with RAW RGB samples (unfiltered on purpose;
        // Flate/JPEG encoding is a later milestone).
        let samples_len = u64::try_from(page.pixels_rgb8.len()).map_err(|_| overflow())?;
        pdf_begin_obj(&mut out, &mut offsets, image_num)?;
        out.extend_from_slice(b"<< /Type /XObject /Subtype /Image /Width ");
        out.extend_from_slice(page.width_px.to_string().as_bytes());
        out.extend_from_slice(b" /Height ");
        out.extend_from_slice(page.height_px.to_string().as_bytes());
        out.extend_from_slice(b" /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length ");
        out.extend_from_slice(samples_len.to_string().as_bytes());
        out.extend_from_slice(b" >>\nstream\n");
        out.extend_from_slice(&page.pixels_rgb8);
        out.extend_from_slice(b"\nendstream\nendobj\n");
    }

    // Info dictionary.
    pdf_begin_obj(&mut out, &mut offsets, info_num)?;
    out.extend_from_slice(b"<<");
    if !meta.title.is_empty() {
        out.extend_from_slice(b" /Title ");
        pdf_push_string(&mut out, &meta.title);
    }
    if !meta.author.is_empty() {
        out.extend_from_slice(b" /Author ");
        pdf_push_string(&mut out, &meta.author);
    }
    out.extend_from_slice(b" >>\nendobj\n");

    // Cross-reference table with exact object offsets.
    let object_total = info_num;
    let size = object_total.checked_add(1).ok_or_else(overflow)?;
    let xref_off = u64::try_from(out.len()).map_err(|_| overflow())?;
    out.extend_from_slice(b"xref\n0 ");
    out.extend_from_slice(size.to_string().as_bytes());
    out.extend_from_slice(b"\n0000000000 65535 f \n");
    let mut num: u64 = 1;
    while num <= object_total {
        let off = offsets.get(usize::try_from(num).map_err(|_| overflow())?).copied().ok_or_else(overflow)?;
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        num = num.checked_add(1).ok_or_else(overflow)?;
    }
    out.extend_from_slice(b"trailer\n<< /Size ");
    out.extend_from_slice(size.to_string().as_bytes());
    out.extend_from_slice(b" /Root 1 0 R /Info ");
    out.extend_from_slice(info_num.to_string().as_bytes());
    out.extend_from_slice(b" 0 R >>\nstartxref\n");
    out.extend_from_slice(xref_off.to_string().as_bytes());
    out.extend_from_slice(b"\n%%EOF");

    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use scancraft_model::{BitDepth, PageSide};

    fn test_page() -> RawPage {
        RawPage { width_px: 2, height_px: 2, dpi: 200, bit_depth: BitDepth::Color, side: PageSide::Front, pixels_rgb8: vec![128u8; 12] }
    }

    fn meta() -> PdfMetadata {
        PdfMetadata { title: "Test".to_owned(), author: "ScanCraft".to_owned() }
    }

    #[test]
    fn pdf_header_and_eof() {
        let bytes = export_pages(&[test_page()], ExportFormat::Pdf, &meta(), 80).unwrap();
        assert!(bytes.starts_with(b"%PDF-1.4"));
        assert!(bytes.ends_with(b"%%EOF"));
    }

    #[test]
    fn tiff_header_magic() {
        let bytes = export_pages(&[test_page()], ExportFormat::Tiff, &meta(), 80).unwrap();
        assert!(bytes.starts_with(b"II*\0"));
    }

    #[test]
    fn tiff_multi_page_links_ifds() {
        let bytes = export_pages(&[test_page(), test_page()], ExportFormat::Tiff, &meta(), 80).unwrap();
        // Second IFD offset (bytes 4..8) must be non-zero so pages chain.
        assert!(bytes.len() > 8);
        assert!(bytes[4..8] != [0, 0, 0, 0]);
    }

    #[test]
    fn bmp_magic() {
        let bytes = export_pages(&[test_page()], ExportFormat::Bmp, &meta(), 80).unwrap();
        assert!(bytes.starts_with(b"BM"));
    }

    #[test]
    fn png_magic() {
        let bytes = export_pages(&[test_page()], ExportFormat::Png, &meta(), 80).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn jpeg_magic() {
        let bytes = export_pages(&[test_page()], ExportFormat::Jpeg, &meta(), 80).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn empty_pages_errors() {
        let err = export_pages(&[], ExportFormat::Pdf, &meta(), 80).unwrap_err();
        assert!(matches!(err, ScanError::Unsupported(_)));
    }

    #[test]
    fn two_pages_to_png_errors() {
        let err = export_pages(&[test_page(), test_page()], ExportFormat::Png, &meta(), 80).unwrap_err();
        assert!(matches!(err, ScanError::Unsupported(_)));
    }

    #[test]
    fn format_from_extension_cases() {
        assert_eq!(format_from_extension("pdf"), Some(ExportFormat::Pdf));
        assert_eq!(format_from_extension(".PDF"), Some(ExportFormat::Pdf));
        assert_eq!(format_from_extension("tif"), Some(ExportFormat::Tiff));
        assert_eq!(format_from_extension("tiff"), Some(ExportFormat::Tiff));
        assert_eq!(format_from_extension("bmp"), Some(ExportFormat::Bmp));
        assert_eq!(format_from_extension("jpg"), Some(ExportFormat::Jpeg));
        assert_eq!(format_from_extension("JPEG"), Some(ExportFormat::Jpeg));
        assert_eq!(format_from_extension("png"), Some(ExportFormat::Png));
        assert_eq!(format_from_extension(".Png"), Some(ExportFormat::Png));
        assert_eq!(format_from_extension("gif"), None);
        assert_eq!(format_from_extension(""), None);
    }
}
