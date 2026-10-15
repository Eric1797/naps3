//! eSCL (AirScan/Mopria) scanner backend over plain HTTP.
//!
//! Port of NAPS2's `EsclScanDriver` (`NAPS2.Sdk/Scan/Internal/Escl/*` in
//! `naps2-in c++-master #For Reference/`); logic ported, never bulk-copied.
//! This project is GPL-2.0-or-later like NAPS2.
//!
//! Transport is plain HTTP only. TLS (https) with per-device trust is a
//! later milestone: eSCL URIs with `https://` are rejected with
//! [`ScanError::Unsupported`]. mDNS/Bonjour discovery is also a later
//! milestone: this backend uses an explicit host/port only
//! ([`EsclOptions`]) and never touches the network in `list_devices`.
//!
//! NAPS2 parity notes (future work, see `scan()` comments):
//! - NAPS2 falls back through a 409 B/W scan when color negotiation fails;
//!   this backend reports the driver error instead.
//! - NAPS2 re-resolves the device IP when it changes mid-job; this backend
//!   pins the host/port parsed from the [`DeviceId`] for the whole job.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use scancraft_model::{BitDepth, PageSize, PaperSource};
use scancraft_scan_core::{CancelToken, Capabilities, DeviceId, DeviceInfo, PageSink, Progress, ScanError, ScanRequest, ScannerBackend};

/// Connection options for one eSCL scanner.
///
/// Explicit host only; mDNS discovery is a documented later milestone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EsclOptions {
    pub host: String,
    pub port: u16,
}

impl Default for EsclOptions {
    fn default() -> Self {
        Self { host: "127.0.0.1".to_owned(), port: 80 }
    }
}

/// eSCL backend. Backend id is `"escl"`.
#[derive(Debug, Clone)]
pub struct EsclBackend {
    pub options: EsclOptions,
}

impl EsclBackend {
    pub fn new(options: EsclOptions) -> Self {
        Self { options }
    }
}

impl ScannerBackend for EsclBackend {
    fn id(&self) -> &'static str {
        "escl"
    }

    fn list_devices(&self, cancel: &CancelToken) -> Result<Vec<DeviceInfo>, ScanError> {
        cancel.check()?;
        let id = DeviceId(format!("escl:{}:{}", self.options.host, self.options.port));
        let display = format!("eSCL Scanner at {}:{}", self.options.host, self.options.port);
        Ok(vec![DeviceInfo { id, backend: "escl", display_name: display }])
    }

    fn capabilities(&self, device: &DeviceId) -> Result<Capabilities, ScanError> {
        let (host, port) = parse_host_port(device)?;
        // 5s-ish budget for the caps fetch (shorter than the 30s scan default).
        let (status, body) = http_request_timeout(&host, port, "GET", "/eSCL/ScannerCapabilities", None, None, 5)?;
        if status != 200 {
            return Err(ScanError::Driver(short_msg(&format!("eSCL ScannerCapabilities failed with HTTP {status}"))));
        }
        let text = String::from_utf8_lossy(&body);
        // parse_caps returns the conservative default when the XML is empty
        // or carries nothing we recognise.
        Ok(parse_caps(text.as_ref()))
    }

    fn scan(&self, request: &ScanRequest, sink: &mut dyn PageSink, cancel: &CancelToken) -> Result<u32, ScanError> {
        cancel.check()?;
        let (host, port) = parse_host_port(&request.device)?;

        // NAPS2 future work: 409 B/W fallback when the scanner rejects the
        // requested color mode. We report the driver error instead.
        // NAPS2 future work: re-resolve the device IP when it changes
        // mid-job. We pin host/port for the whole job instead.
        let dpi = request.dpi.clamp(75, 600);
        let color_mode = match request.bit_depth {
            BitDepth::Color => "RGB24",
            BitDepth::Grayscale => "Grayscale8",
            BitDepth::BlackWhite => "BlackAndWhite1",
        };
        let (input_source, duplex) = match request.paper_source {
            PaperSource::Flatbed | PaperSource::Auto => ("Platen", false),
            PaperSource::Feeder => ("Feeder", false),
            PaperSource::Duplex => ("Feeder", true),
        };
        let settings = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n\
             <scan:ScanSettings xmlns:scan=\"http://schemas.hp.com/imaging/escl/2011/05/18\">\
             <scan:XResolution>{dpi}</scan:XResolution>\
             <scan:YResolution>{dpi}</scan:YResolution>\
             <scan:ColorMode>{color_mode}</scan:ColorMode>\
             <scan:InputSource>{input_source}</scan:InputSource>\
             <!-- Duplex: {duplex} -->\
             </scan:ScanSettings>"
        );

        let (status, headers, _body) =
            http_request_full(&host, port, "POST", "/eSCL/ScanJobs", Some(settings.as_bytes()), Some("text/xml; charset=utf-8"), 30)?;
        if status != 201 && status != 200 {
            return Err(ScanError::Driver(short_msg(&format!("eSCL ScanJobs failed with HTTP {status}"))));
        }
        let location =
            header_value(&headers, "location").ok_or_else(|| ScanError::Driver("eSCL ScanJobs response has no Location header".to_owned()))?;
        let job_path = location_to_path(&location)?;

        let mut pages: u32 = 0;
        for _ in 0..100 {
            cancel.check()?;
            let base = job_path.trim_end_matches('/');
            let next_path = format!("{base}/NextDocument");
            let (status, body) = http_request(&host, port, "GET", &next_path, None, None)?;
            if status == 404 {
                break;
            }
            if status != 200 {
                return Err(ScanError::Driver(short_msg(&format!("eSCL NextDocument failed with HTTP {status}"))));
            }
            cancel.check()?;
            let page = scancraft_imaging::decode_jpeg(&body).map_err(|e| ScanError::Driver(short_msg(&format!("JPEG decode failed: {e}"))))?;
            page.validate()?;
            sink.page(page)?;
            pages = pages.saturating_add(1);
            sink.progress(Progress { pages_done: pages, stage: "scan" });
        }

        // Best-effort job cleanup; ignore errors.
        let _ignored = http_request(&host, port, "DELETE", &job_path, None, None);
        Ok(pages)
    }
}

/// Conservative fallback when caps XML parses but carries nothing we
/// recognise (documented fallback).
fn default_caps() -> Capabilities {
    Capabilities {
        sources: vec![PaperSource::Flatbed],
        dpis: vec![100, 200, 300],
        bit_depths: vec![BitDepth::Color, BitDepth::Grayscale, BitDepth::BlackWhite],
        page_sizes: vec![PageSize::Letter, PageSize::A4],
        duplex: false,
    }
}

/// Parse eSCL `ScannerCapabilities` XML in a namespace-tolerant way: match
/// on local element names only (prefixes like `scan:`/`pwg:` ignored).
/// Never fails; returns [`default_caps`] when nothing is recognised.
pub(crate) fn parse_caps(xml: &str) -> Capabilities {
    if xml.trim().is_empty() {
        return default_caps();
    }
    let mut has_platen = false;
    let mut has_feeder = false;
    let mut duplex = false;
    let mut dpis: Vec<u32> = Vec::new();

    let mut reader = quick_xml::reader::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf: Vec<u8> = Vec::new();
    let mut current = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                if local.contains("platen") {
                    has_platen = true;
                }
                if local.contains("adf") || local.contains("feeder") {
                    has_feeder = true;
                }
                if local.contains("duplex") {
                    duplex = true;
                    has_feeder = true;
                }
                current = local;
            }
            Ok(quick_xml::events::Event::Empty(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                if local.contains("platen") {
                    has_platen = true;
                }
                if local.contains("adf") || local.contains("feeder") {
                    has_feeder = true;
                }
                if local.contains("duplex") {
                    duplex = true;
                    has_feeder = true;
                }
                current.clear();
            }
            Ok(quick_xml::events::Event::Text(e)) => {
                let decoded = match e.decode() {
                    Ok(t) => t.into_owned(),
                    Err(_) => {
                        current.clear();
                        buf.clear();
                        continue;
                    }
                };
                let t = decoded.trim().to_ascii_lowercase();
                if current == "inputsource" {
                    if t.contains("platen") {
                        has_platen = true;
                    }
                    if t.contains("feeder") || t.contains("adf") {
                        has_feeder = true;
                    }
                    if t.contains("duplex") {
                        duplex = true;
                        has_feeder = true;
                    }
                }
                if current == "duplex" && (t == "true" || t == "1" || t.contains("duplex")) {
                    duplex = true;
                    has_feeder = true;
                }
                if (current == "xresolution" || current == "yresolution" || current == "resolution" || current == "discreteresolution")
                    && let Ok(v) = t.parse::<u32>()
                    && (50..=4800).contains(&v)
                    && !dpis.contains(&v)
                {
                    dpis.push(v);
                }
            }
            Ok(quick_xml::events::Event::End(_)) => {
                current.clear();
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    if !has_platen && !has_feeder && dpis.is_empty() && !duplex {
        return default_caps();
    }
    let mut sources = Vec::new();
    if has_platen {
        sources.push(PaperSource::Flatbed);
    }
    if has_feeder && !sources.contains(&PaperSource::Feeder) {
        sources.push(PaperSource::Feeder);
    }
    if duplex {
        if !sources.contains(&PaperSource::Feeder) {
            sources.push(PaperSource::Feeder);
        }
        if !sources.contains(&PaperSource::Duplex) {
            sources.push(PaperSource::Duplex);
        }
    }
    if sources.is_empty() {
        sources.push(PaperSource::Flatbed);
    }
    if dpis.is_empty() {
        dpis = vec![100, 200, 300];
    } else {
        dpis.sort_unstable();
    }
    Capabilities {
        sources,
        dpis,
        bit_depths: vec![BitDepth::Color, BitDepth::Grayscale, BitDepth::BlackWhite],
        page_sizes: vec![PageSize::Letter, PageSize::A4],
        duplex,
    }
}

/// Parse `"escl:<host>:<port>"` back into host/port. Malformed ids are
/// [`ScanError::Unsupported`].
fn parse_host_port(device: &DeviceId) -> Result<(String, u16), ScanError> {
    let rest = device.0.strip_prefix("escl:").ok_or_else(|| ScanError::Unsupported("malformed eSCL device id".to_owned()))?;
    if rest.is_empty() {
        return Err(ScanError::Unsupported("malformed eSCL device id".to_owned()));
    }
    let (host_part, port_part) = rest.rsplit_once(':').ok_or_else(|| ScanError::Unsupported("malformed eSCL device id".to_owned()))?;
    if host_part.is_empty() || port_part.is_empty() {
        return Err(ScanError::Unsupported("malformed eSCL device id".to_owned()));
    }
    // Accept bracketed IPv6 (`[::1]`); strip brackets when present.
    let host = if host_part.starts_with('[') && host_part.ends_with(']') && host_part.len() >= 2 {
        host_part.get(1..host_part.len().saturating_sub(1)).unwrap_or("").to_owned()
    } else {
        host_part.to_owned()
    };
    if host.is_empty() {
        return Err(ScanError::Unsupported("malformed eSCL device id".to_owned()));
    }
    let port: u16 = port_part.parse().map_err(|_| ScanError::Unsupported("malformed eSCL device id".to_owned()))?;
    Ok((host, port))
}

fn short_msg(s: &str) -> String {
    const CAP: usize = 200;
    if s.len() <= CAP { s.to_owned() } else { s.chars().take(CAP).collect() }
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    for (k, v) in headers {
        if k == name {
            return Some(v.clone());
        }
    }
    None
}

/// Resolve a `Location` header value to an origin-form path.
///
/// Accepts absolute paths (`/eSCL/ScanJobs/1`) and absolute URIs
/// (`http://host:port/eSCL/ScanJobs/1`); strips query/fragment.
fn location_to_path(location: &str) -> Result<String, ScanError> {
    let loc = location.trim();
    if loc.is_empty() {
        return Err(ScanError::Driver("eSCL job Location is empty".to_owned()));
    }
    let without_scheme = if let Some(rest) = loc.strip_prefix("http://") {
        rest
    } else if let Some(rest) = loc.strip_prefix("https://") {
        let _ = rest;
        return Err(ScanError::Unsupported("eSCL https URIs need the TLS milestone".to_owned()));
    } else {
        loc
    };
    let path = if loc.starts_with('/') {
        loc.to_owned()
    } else if without_scheme.len() != loc.len() {
        // Absolute http URI: path starts at the first '/' after the host.
        match without_scheme.find('/') {
            Some(i) => without_scheme.get(i..).unwrap_or("/").to_owned(),
            None => {
                return Err(ScanError::Driver("eSCL job Location has no path".to_owned()));
            }
        }
    } else {
        // Relative reference: treat as rooted.
        format!("/{loc}")
    };
    // Strip query/fragment.
    let mut end = path.len();
    for (i, ch) in path.char_indices() {
        if ch == '?' || ch == '#' {
            end = i;
            break;
        }
    }
    let clean = path.get(..end).unwrap_or("/").to_owned();
    if clean.is_empty() {
        return Err(ScanError::Driver("eSCL job Location has no path".to_owned()));
    }
    Ok(clean)
}

/// Minimal blocking HTTP/1.1 client over [`TcpStream`]: GET/POST/DELETE with
/// `Content-Length` and chunked responses. Timeouts via `set_read_timeout`.
/// IO errors map to [`ScanError::Driver`] with short messages.
fn http_request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    content_type: Option<&str>,
) -> Result<(u16, Vec<u8>), ScanError> {
    let (status, _headers, data) = http_request_full(host, port, method, path, body, content_type, 30)?;
    Ok((status, data))
}

fn http_request_timeout(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    content_type: Option<&str>,
    read_secs: u64,
) -> Result<(u16, Vec<u8>), ScanError> {
    let (status, _headers, data) = http_request_full(host, port, method, path, body, content_type, read_secs)?;
    Ok((status, data))
}

/// Parsed HTTP response: status, lowercase headers, body.
type HttpResponse = (u16, Vec<(String, String)>, Vec<u8>);

fn http_request_full(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    content_type: Option<&str>,
    read_secs: u64,
) -> Result<HttpResponse, ScanError> {
    if method.is_empty() || path.is_empty() || !path.starts_with('/') {
        return Err(ScanError::Unsupported("bad eSCL request path".to_owned()));
    }
    for bad in ["\r", "\n"] {
        if host.contains(bad) || path.contains(bad) || method.contains(bad) {
            return Err(ScanError::Unsupported("bad eSCL request".to_owned()));
        }
    }
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(addr.as_str()).map_err(|e| ScanError::Driver(short_msg(&format!("eSCL connect failed: {e}"))))?;
    let read_to = Duration::from_secs(read_secs.max(1));
    stream.set_read_timeout(Some(read_to)).map_err(|e| ScanError::Driver(short_msg(&format!("eSCL timeout setup failed: {e}"))))?;
    stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(|e| ScanError::Driver(short_msg(&format!("eSCL timeout setup failed: {e}"))))?;

    let body_len = body.map_or(0, <[u8]>::len);
    let host_header = if port == 80 { host.to_owned() } else { format!("{host}:{port}") };
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n");
    if let Some(ct) = content_type {
        if ct.contains('\r') || ct.contains('\n') {
            return Err(ScanError::Unsupported("bad eSCL content type".to_owned()));
        }
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    head.push_str(&format!("Content-Length: {body_len}\r\n\r\n"));
    stream.write_all(head.as_bytes()).map_err(|e| ScanError::Driver(short_msg(&format!("eSCL send failed: {e}"))))?;
    if let Some(b) = body {
        stream.write_all(b).map_err(|e| ScanError::Driver(short_msg(&format!("eSCL send failed: {e}"))))?;
    }
    let mut raw: Vec<u8> = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| ScanError::Driver(short_msg(&format!("eSCL read failed: {e}"))))?;

    let end = find_header_end(&raw).ok_or_else(|| ScanError::Driver("eSCL bad HTTP response".to_owned()))?;
    let body_bytes = raw.get(end..).unwrap_or(&[]).to_vec();
    let header_text = String::from_utf8_lossy(raw.get(..end).unwrap_or(&[])).into_owned();
    let mut lines = header_text.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| ScanError::Driver("eSCL bad HTTP status".to_owned()))?;
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let chunked = headers.iter().any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let data = if chunked {
        decode_chunked(&body_bytes)?
    } else {
        let mut data = body_bytes;
        for (k, v) in &headers {
            if k != "content-length" {
                continue;
            }
            if let Ok(n) = v.trim().parse::<usize>() {
                data.truncate(data.len().min(n));
            }
            break;
        }
        data
    };
    Ok((status, headers, data))
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|w| w.first() == Some(&b'\r') && w.get(1) == Some(&b'\n') && w.get(2) == Some(&b'\r') && w.get(3) == Some(&b'\n'))
        .map(|p| p.saturating_add(4))
}

fn decode_chunked(data: &[u8]) -> Result<Vec<u8>, ScanError> {
    let mut out = Vec::new();
    let mut i: usize = 0;
    loop {
        // Find the end of the chunk-size line.
        let mut line_end: Option<usize> = None;
        let mut j = i;
        while j < data.len() {
            if data.get(j) == Some(&b'\r') && data.get(j.saturating_add(1)) == Some(&b'\n') {
                line_end = Some(j);
                break;
            }
            j = j.saturating_add(1);
            if j == 0 {
                break;
            }
        }
        let le = match line_end {
            Some(v) => v,
            None => {
                return Err(ScanError::Driver("eSCL bad chunked body".to_owned()));
            }
        };
        let line = String::from_utf8_lossy(data.get(i..le).unwrap_or(&[]));
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| ScanError::Driver("eSCL bad chunked body".to_owned()))?;
        if size == 0 {
            break;
        }
        if size > 256 * 1024 * 1024 {
            return Err(ScanError::Driver("eSCL chunk too large".to_owned()));
        }
        let start = le.saturating_add(2);
        let stop = start.saturating_add(size);
        let chunk = data.get(start..stop).ok_or_else(|| ScanError::Driver("eSCL bad chunked body".to_owned()))?;
        out.extend_from_slice(chunk);
        // Expect trailing CRLF after the chunk.
        let after = stop;
        if data.get(after) != Some(&b'\r') || data.get(after.saturating_add(1)) != Some(&b'\n') {
            return Err(ScanError::Driver("eSCL bad chunked body".to_owned()));
        }
        i = after.saturating_add(2);
        if i >= data.len() && size != 0 {
            break;
        }
        if out.len() > 512 * 1024 * 1024 {
            return Err(ScanError::Driver("eSCL response too large".to_owned()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const ADF_CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScannerCapabilities xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/18" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
  <scan:Platen><scan:PlatenInputCaps><scan:MinWidth>1</scan:MinWidth></scan:PlatenInputCaps></scan:Platen>
  <scan:Adf>
    <scan:AdfSimplexInputCaps><scan:MinWidth>1</scan:MinWidth></scan:AdfSimplexInputCaps>
    <scan:AdfDuplexInputCaps><scan:MinWidth>1</scan:MinWidth></scan:AdfDuplexInputCaps>
  </scan:Adf>
  <scan:SupportedResolutions>
    <scan:DiscreteResolutions>
      <scan:DiscreteResolution><scan:XResolution>75</scan:XResolution><scan:YResolution>75</scan:YResolution></scan:DiscreteResolution>
      <scan:DiscreteResolution><scan:XResolution>150</scan:XResolution><scan:YResolution>150</scan:YResolution></scan:DiscreteResolution>
      <scan:DiscreteResolution><scan:XResolution>300</scan:XResolution><scan:YResolution>300</scan:YResolution></scan:DiscreteResolution>
    </scan:DiscreteResolutions>
  </scan:SupportedResolutions>
  <scan:Duplex>true</scan:Duplex>
</scan:ScannerCapabilities>"#;

    // 1x1 white JPEG (base64) served by the mock eSCL server; decoded by
    // scancraft-imaging on the client side. Embedded to stay hermetic
    // instead of depending on an imaging encoder.
    const MINI_JPEG_B64: &str = "/9j/4AAQSkZJRgABAQEASABIAAD/2wBDAP//////////////////////////////////////////////////////////////////////////////////////2wBDAf//////////////////////////////////////////////////////////////////////////////////////wAARCAABAAEDASIAAhEBAxEB/8QAFAABAAAAAAAAAAAAAAAAAAAAAP/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AK//9k=";

    fn b64_decode(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut acc: u32 = 0;
        let mut bits: u32 = 0;
        for ch in s.chars() {
            let v = match ch {
                'A'..='Z' => (ch as u32) - ('A' as u32),
                'a'..='z' => (ch as u32) - ('a' as u32) + 26,
                '0'..='9' => (ch as u32) - ('0' as u32) + 52,
                '+' => 62,
                '/' => 63,
                '=' => continue,
                _ => continue,
            };
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((acc >> bits) & 0xFF) as u8);
            }
        }
        out
    }

    fn read_http_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
        let mut buf = vec![0u8; 65536];
        let mut got: usize = 0;
        // Read until end of headers.
        loop {
            let n = stream.read(&mut buf[got..]).unwrap_or(0);
            if n == 0 {
                break;
            }
            got += n;
            if find_header_end(&buf[..got]).is_some() || got >= buf.len() {
                break;
            }
        }
        let end = find_header_end(&buf[..got]).unwrap_or(got);
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        // Honor Content-Length for POST bodies.
        let mut content_len: usize = 0;
        for line in head.lines().skip(1) {
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                content_len = v.trim().parse::<usize>().unwrap_or(0);
            }
        }
        let mut body = buf[end..got].to_vec();
        while body.len() < content_len {
            let n = stream.read(&mut buf[..]).unwrap_or(0);
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        }
        (head, body)
    }

    #[test]
    fn caps_with_adf_duplex_and_discrete_dpis() {
        let caps = parse_caps(ADF_CAPS);
        assert!(caps.sources.contains(&PaperSource::Flatbed));
        assert!(caps.sources.contains(&PaperSource::Feeder));
        assert!(caps.sources.contains(&PaperSource::Duplex));
        assert!(caps.duplex);
        assert!(caps.dpis.contains(&75));
        assert!(caps.dpis.contains(&150));
        assert!(caps.dpis.contains(&300));
    }

    #[test]
    fn caps_empty_xml_falls_back_to_default() {
        assert_eq!(parse_caps(""), default_caps());
        assert_eq!(parse_caps("   \n  "), default_caps());
        assert_eq!(parse_caps("<foo/>"), default_caps());
    }

    #[test]
    fn http_request_roundtrip_against_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let _ = read_http_request(&mut s);
                let body = b"hello-escl";
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(resp.as_bytes());
                let _ = s.write_all(body);
            }
        });
        let (status, body) = http_request("127.0.0.1", port, "GET", "/eSCL/X", None, None).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello-escl");
    }

    #[test]
    fn full_scan_against_mock_escl_server() {
        let jpeg = b64_decode(MINI_JPEG_B64);
        assert!(!jpeg.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = jpeg.clone();
        std::thread::spawn(move || {
            let mut docs_served = 0u32;
            for _ in 0..8 {
                let accepted = listener.accept();
                let Ok((mut s, _)) = accepted else {
                    break;
                };
                let (head, _body) = read_http_request(&mut s);
                let line = head.lines().next().unwrap_or("").to_owned();
                if line.starts_with("POST /eSCL/ScanJobs") {
                    let resp = "HTTP/1.1 201 Created\r\nLocation: /eSCL/ScanJobs/1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = s.write_all(resp.as_bytes());
                } else if line.contains("/NextDocument") {
                    if docs_served == 0 {
                        docs_served += 1;
                        let resp =
                            format!("HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", served.len());
                        let _ = s.write_all(resp.as_bytes());
                        let _ = s.write_all(&served);
                    } else {
                        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = s.write_all(resp.as_bytes());
                    }
                } else if line.starts_with("DELETE ") {
                    let resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = s.write_all(resp.as_bytes());
                } else {
                    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = s.write_all(resp.as_bytes());
                }
            }
        });

        let backend = EsclBackend::new(EsclOptions { host: "127.0.0.1".to_owned(), port });
        let req = ScanRequest {
            device: DeviceId(format!("escl:127.0.0.1:{port}")),
            paper_source: PaperSource::Flatbed,
            page_size: PageSize::Letter,
            dpi: 200,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        };
        struct Collect {
            pages: Vec<scancraft_scan_core::RawPage>,
        }
        impl PageSink for Collect {
            fn page(&mut self, page: scancraft_scan_core::RawPage) -> Result<(), ScanError> {
                self.pages.push(page);
                Ok(())
            }
            fn progress(&mut self, _p: Progress) {}
        }
        let mut sink = Collect { pages: Vec::new() };
        let n = backend.scan(&req, &mut sink, &CancelToken::new()).unwrap();
        assert_eq!(n, 1);
        assert_eq!(sink.pages.len(), 1);
    }

    #[test]
    fn malformed_device_id_is_unsupported() {
        let backend = EsclBackend::new(EsclOptions::default());
        let err = backend.capabilities(&DeviceId("bogus".to_owned())).unwrap_err();
        assert!(matches!(err, ScanError::Unsupported(_)));
        let req = ScanRequest {
            device: DeviceId("escl:".to_owned()),
            paper_source: PaperSource::Flatbed,
            page_size: PageSize::Letter,
            dpi: 200,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        };
        struct Noop;
        impl PageSink for Noop {
            fn page(&mut self, _p: scancraft_scan_core::RawPage) -> Result<(), ScanError> {
                Ok(())
            }
            fn progress(&mut self, _p: Progress) {}
        }
        let mut sink = Noop;
        let err = backend.scan(&req, &mut sink, &CancelToken::new()).unwrap_err();
        assert!(matches!(err, ScanError::Unsupported(_)));
    }
}
