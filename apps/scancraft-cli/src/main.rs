//! Headless ScanCraft CLI: `devices`, `profiles list`, and `scan` + export.
//!
//! Port of the NAPS2 headless surface (`NAPS2.Sdk` scan + console tooling in
//! `naps2-in c++-master #For Reference/`); logic ported, never bulk-copied.
//! This project is GPL-2.0-or-later like NAPS2.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::PathBuf;

use scancraft_engine::Engine;
use scancraft_export::{PdfMetadata, export_pages, format_from_extension};
use scancraft_model::{BitDepth, PageSize, PaperSource};
use scancraft_profiles::ProfileStore;
use scancraft_scan_core::{CancelToken, DeviceId, PageSink, Progress, RawPage, ScanError, ScanRequest, ScannerBackend};
use scancraft_scan_escl::{EsclBackend, EsclOptions};
use scancraft_scan_mock::MockScanner;
use scancraft_scan_sane::{SaneBackend, SaneOptions};
use scancraft_scan_twain::{TwainBackend, TwainOptions};
use scancraft_scan_wia::{WiaBackend, WiaOptions};

struct EngineSink<'a> {
    engine: &'a mut Engine,
    doc: u64,
}

impl PageSink for EngineSink<'_> {
    fn page(&mut self, page: RawPage) -> Result<(), ScanError> {
        match self.engine.document_mut(self.doc) {
            Some(d) => d.append_page(page).map(|_| ()),
            None => Err(ScanError::Driver("document went away".to_owned())),
        }
    }

    fn progress(&mut self, _progress: Progress) {}
}

fn usage() -> &'static str {
    "usage:\n  scancraft-cli devices [--backend mock|wia|twain|sane|escl] [--escl-host H] [--escl-port P]\n  scancraft-cli profiles [--store PATH] list\n  scancraft-cli scan [--backend B] [--device ID] [--dpi N] [--profile NAME --store PATH] [--out FILE] [--format pdf|tiff|bmp|jpeg|png] [--title T] [--author A] [--escl-host H] [--escl-port P]"
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), ScanError> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    match cmd {
        "devices" => cmd_devices(&args),
        "profiles" => cmd_profiles(&args),
        "scan" => cmd_scan(&args),
        _ => {
            eprintln!("{}", usage());
            Err(ScanError::Unsupported("unknown command".to_owned()))
        }
    }
}

fn valid_backend(name: &str) -> bool {
    matches!(name, "mock" | "wia" | "twain" | "sane" | "escl")
}

fn make_backend(name: &str, escl_host: &str, escl_port: u16) -> Result<Box<dyn ScannerBackend>, ScanError> {
    match name {
        "mock" => Ok(Box::new(MockScanner::new())),
        "wia" => Ok(Box::new(WiaBackend::new(WiaOptions::default()))),
        "twain" => Ok(Box::new(TwainBackend::new(TwainOptions::default()))),
        "sane" => Ok(Box::new(SaneBackend::new(SaneOptions::default()))),
        "escl" => Ok(Box::new(EsclBackend::new(EsclOptions { host: escl_host.to_owned(), port: escl_port }))),
        _ => Err(ScanError::Unsupported(format!("unknown backend: {name}"))),
    }
}

fn parse_u16_flag(value: &str, flag: &str) -> Result<u16, ScanError> {
    value.parse::<u16>().map_err(|_| ScanError::Unsupported(format!("bad {flag} value")))
}

fn cmd_devices(args: &[String]) -> Result<(), ScanError> {
    let mut backend: Option<String> = None;
    let mut escl_host = "127.0.0.1".to_owned();
    let mut escl_port: u16 = 80;
    let mut i: usize = 2;
    while i < args.len() {
        let flag = args.get(i).map(String::as_str).unwrap_or("");
        if flag == "--backend" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --backend value".to_owned()))?;
            if !valid_backend(v) {
                return Err(ScanError::Unsupported(format!("unknown backend: {v}")));
            }
            backend = Some(v.clone());
            i = i.saturating_add(2);
        } else if flag == "--escl-host" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --escl-host value".to_owned()))?;
            escl_host = v.clone();
            i = i.saturating_add(2);
        } else if flag == "--escl-port" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --escl-port value".to_owned()))?;
            escl_port = parse_u16_flag(v, "--escl-port")?;
            i = i.saturating_add(2);
        } else {
            return Err(ScanError::Unsupported(format!("unknown flag: {flag}")));
        }
    }

    if let Some(name) = backend {
        // Single-backend mode: errors propagate (exit 1).
        let be = make_backend(&name, &escl_host, escl_port)?;
        let devs = be.list_devices(&CancelToken::new())?;
        for d in devs {
            println!("{}\t{}", d.id.0, d.display_name);
        }
        return Ok(());
    }

    // All-backends mode: per-backend failures go to stderr, others still list.
    let names = ["mock", "wia", "twain", "sane", "escl"];
    let mut ok_count: u32 = 0;
    for name in names {
        let be = make_backend(name, &escl_host, escl_port)?;
        match be.list_devices(&CancelToken::new()) {
            Ok(devs) => {
                ok_count = ok_count.saturating_add(1);
                for d in devs {
                    println!("{}\t{}", d.id.0, d.display_name);
                }
            }
            Err(e) => {
                eprintln!("backend error [{name}]: {e}");
            }
        }
    }
    if ok_count == 0 {
        return Err(ScanError::Driver("all backends failed".to_owned()));
    }
    Ok(())
}

fn cmd_profiles(args: &[String]) -> Result<(), ScanError> {
    let mut store: Option<String> = None;
    let mut list = false;
    let mut i: usize = 2;
    while i < args.len() {
        let flag = args.get(i).map(String::as_str).unwrap_or("");
        if flag == "--store" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --store value".to_owned()))?;
            store = Some(v.clone());
            i = i.saturating_add(2);
        } else if flag == "list" {
            list = true;
            i = i.saturating_add(1);
        } else {
            return Err(ScanError::Unsupported(format!("unknown flag: {flag}")));
        }
    }
    if !list {
        eprintln!("{}", usage());
        return Err(ScanError::Unsupported("profiles needs `list`".to_owned()));
    }
    let path = store.ok_or_else(|| ScanError::Unsupported("missing --store".to_owned()))?;
    let ps = ProfileStore::new(PathBuf::from(path));
    let all = ps.load()?;
    for p in all {
        if p.is_default {
            println!("{} (default)", p.name);
        } else {
            println!("{}", p.name);
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct ScanFlags {
    backend: String,
    device: Option<String>,
    dpi: Option<u32>,
    profile: Option<String>,
    store: Option<String>,
    out: Option<String>,
    format: Option<String>,
    title: String,
    author: String,
    escl_host: String,
    escl_port: u16,
}

fn cmd_scan(args: &[String]) -> Result<(), ScanError> {
    let mut f = ScanFlags { backend: "mock".to_owned(), escl_host: "127.0.0.1".to_owned(), escl_port: 80, ..ScanFlags::default() };
    let mut i: usize = 2;
    while i < args.len() {
        let flag = args.get(i).map(String::as_str).unwrap_or("");
        if flag == "--backend" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --backend value".to_owned()))?;
            if !valid_backend(v) {
                return Err(ScanError::Unsupported(format!("unknown backend: {v}")));
            }
            f.backend = v.clone();
            i = i.saturating_add(2);
        } else if flag == "--device" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --device value".to_owned()))?;
            f.device = Some(v.clone());
            i = i.saturating_add(2);
        } else if flag == "--dpi" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --dpi value".to_owned()))?;
            let n: u32 = v.parse().map_err(|_| ScanError::Unsupported("bad --dpi value".to_owned()))?;
            f.dpi = Some(n);
            i = i.saturating_add(2);
        } else if flag == "--profile" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --profile value".to_owned()))?;
            f.profile = Some(v.clone());
            i = i.saturating_add(2);
        } else if flag == "--store" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --store value".to_owned()))?;
            f.store = Some(v.clone());
            i = i.saturating_add(2);
        } else if flag == "--out" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --out value".to_owned()))?;
            f.out = Some(v.clone());
            i = i.saturating_add(2);
        } else if flag == "--format" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --format value".to_owned()))?;
            f.format = Some(v.clone());
            i = i.saturating_add(2);
        } else if flag == "--title" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --title value".to_owned()))?;
            f.title = v.clone();
            i = i.saturating_add(2);
        } else if flag == "--author" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --author value".to_owned()))?;
            f.author = v.clone();
            i = i.saturating_add(2);
        } else if flag == "--escl-host" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --escl-host value".to_owned()))?;
            f.escl_host = v.clone();
            i = i.saturating_add(2);
        } else if flag == "--escl-port" {
            let v = args.get(i.saturating_add(1)).ok_or_else(|| ScanError::Unsupported("missing --escl-port value".to_owned()))?;
            f.escl_port = parse_u16_flag(v, "--escl-port")?;
            i = i.saturating_add(2);
        } else {
            return Err(ScanError::Unsupported(format!("unknown flag: {flag}")));
        }
    }

    let backend = make_backend(&f.backend, &f.escl_host, f.escl_port)?;
    let mut quality: u8 = 75;

    let request = if let Some(profile_name) = f.profile {
        let store_path = f.store.ok_or_else(|| ScanError::Unsupported("missing --store for --profile".to_owned()))?;
        let ps = ProfileStore::new(PathBuf::from(store_path));
        let all = ps.load()?;
        let found =
            all.into_iter().find(|p| p.name == profile_name).ok_or_else(|| ScanError::Unsupported(format!("profile not found: {profile_name}")))?;
        let profile = found.profile;
        if (0..=100).contains(&profile.quality) {
            quality = profile.quality as u8;
        }
        let device = f.device.unwrap_or(profile.device_id.clone());
        if device.is_empty() {
            return Err(ScanError::NoDevice);
        }
        let dpi = f.dpi.unwrap_or(profile.dpi);
        if !(50..=4800).contains(&dpi) {
            return Err(ScanError::Unsupported("dpi must be between 50 and 4800".to_owned()));
        }
        ScanRequest {
            device: DeviceId(device),
            paper_source: profile.paper_source.to_paper_source(),
            page_size: profile.page_size,
            dpi,
            bit_depth: profile.bit_depth,
            brightness: profile.brightness,
            contrast: profile.contrast,
        }
    } else {
        let device = match f.device {
            Some(d) => d,
            None => {
                if f.backend == "mock" {
                    "mock:feeder".to_owned()
                } else if f.backend == "escl" {
                    format!("escl:{}:{}", f.escl_host, f.escl_port)
                } else {
                    // Fall back to the first discoverable device.
                    let devs = backend.list_devices(&CancelToken::new())?;
                    devs.into_iter().next().map(|d| d.id.0).ok_or(ScanError::NoDevice)?
                }
            }
        };
        if device.is_empty() {
            return Err(ScanError::NoDevice);
        }
        let dpi = f.dpi.unwrap_or(200);
        if !(50..=4800).contains(&dpi) {
            return Err(ScanError::Unsupported("dpi must be between 50 and 4800".to_owned()));
        }
        ScanRequest {
            device: DeviceId(device),
            paper_source: PaperSource::Feeder,
            page_size: PageSize::Letter,
            dpi,
            bit_depth: BitDepth::Color,
            brightness: 0,
            contrast: 0,
        }
    };

    let mut engine = Engine::new();
    let doc = engine.create_document();
    let mut sink = EngineSink { engine: &mut engine, doc };
    let n = backend.scan(&request, &mut sink, &CancelToken::new())?;
    let count = match sink.engine.document(doc) {
        Some(d) => d.pages().len(),
        None => 0,
    };
    println!("scanned {n} pages into document {doc} ({count} stored)");

    if let Some(out) = f.out {
        let fmt = if let Some(name) = f.format {
            match name.to_ascii_lowercase().as_str() {
                "pdf" => scancraft_export::ExportFormat::Pdf,
                "tiff" | "tif" => scancraft_export::ExportFormat::Tiff,
                "bmp" => scancraft_export::ExportFormat::Bmp,
                "jpeg" | "jpg" => scancraft_export::ExportFormat::Jpeg,
                "png" => scancraft_export::ExportFormat::Png,
                _ => {
                    return Err(ScanError::Unsupported(format!("unknown --format: {name}")));
                }
            }
        } else {
            let ext = out.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            format_from_extension(&ext).ok_or_else(|| ScanError::Unsupported("cannot infer format from --out extension".to_owned()))?
        };
        let raws: Vec<RawPage> = match sink.engine.document(doc) {
            Some(d) => d.pages().iter().map(|p| p.raw.clone()).collect(),
            None => Vec::new(),
        };
        let meta = PdfMetadata { title: f.title, author: f.author };
        let bytes = export_pages(&raws, fmt, &meta, quality)?;
        let path = PathBuf::from(&out);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| ScanError::Io(e.to_string()))?;
        }
        std::fs::write(&path, bytes).map_err(|e| ScanError::Io(e.to_string()))?;
    }
    Ok(())
}
