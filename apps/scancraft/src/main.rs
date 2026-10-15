//! ScanCraft desktop app: scan, organize, and export documents.
//!
//! egui GUI in the PdfCraft visual language (tokens/palette styling arrives as
//! a dedicated milestone; layout and workflow mirror NAPS2's desktop form).
//! All hardware work runs on a worker thread; the UI only polls an mpsc
//! channel. Ported from NAPS2 desktop vocabulary (GPL-2.0-or-later).

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::sync::mpsc::{Receiver, Sender};

use scancraft_engine::Engine;
use scancraft_export::{ExportFormat, PdfMetadata, export_pages, format_from_extension};
use scancraft_imaging::Transform;
use scancraft_model::{BitDepth, PageSize, PaperSource, ScanSource};
use scancraft_profiles::{ProfileStore, StoredProfile};
use scancraft_scan_core::{CancelToken, DeviceId, DeviceInfo, PageSink, Progress, RawPage, ScanError, ScanRequest, ScannerBackend};
use scancraft_scan_escl::{EsclBackend, EsclOptions};
use scancraft_scan_mock::MockScanner;
use scancraft_scan_sane::{SaneBackend, SaneOptions};
use scancraft_scan_twain::{TwainBackend, TwainOptions};
use scancraft_scan_wia::{WiaBackend, WiaOptions};

const BACKENDS: [&str; 5] = ["mock", "wia", "twain", "sane", "escl"];
const DPIS: [u32; 6] = [100, 150, 200, 300, 600, 1200];
const PAGE_SIZES: [PageSize; 4] = [PageSize::Letter, PageSize::A4, PageSize::Legal, PageSize::A5];
const EXPORT_NAMES: [&str; 5] = ["PDF", "TIFF", "PNG", "JPEG", "BMP"];
const THUMB_W: u32 = 300;
const PREVIEW_W: u32 = 720;

/// Snapshot of the settings a worker-thread scan runs with.
struct ScanJob {
    backend: String,
    device: String,
    dpi: u32,
    source: PaperSource,
    bit_depth: BitDepth,
    page_size: PageSize,
    escl_host: String,
    escl_port: u16,
}

/// Events the worker thread streams to the UI.
enum UiEvent {
    Page(RawPage),
    Progress(u32),
    Done(Result<u32, String>),
}

struct TxSink {
    tx: Sender<UiEvent>,
}

impl PageSink for TxSink {
    fn page(&mut self, page: RawPage) -> Result<(), ScanError> {
        self.tx.send(UiEvent::Page(page)).map_err(|_| ScanError::Cancelled)
    }

    fn progress(&mut self, progress: Progress) {
        let _ = self.tx.send(UiEvent::Progress(progress.pages_done));
    }
}

fn make_backend(name: &str, host: &str, port: u16) -> Result<Box<dyn ScannerBackend>, ScanError> {
    match name {
        "mock" => Ok(Box::new(MockScanner::new())),
        "wia" => Ok(Box::new(WiaBackend::new(WiaOptions::default()))),
        "twain" => Ok(Box::new(TwainBackend::new(TwainOptions::default()))),
        "sane" => Ok(Box::new(SaneBackend::new(SaneOptions::default()))),
        "escl" => Ok(Box::new(EsclBackend::new(EsclOptions { host: host.to_owned(), port }))),
        _ => Err(ScanError::Unsupported(format!("unknown backend: {name}"))),
    }
}

fn run_scan_job(job: ScanJob, tx: Sender<UiEvent>, cancel: CancelToken) {
    let backend = match make_backend(&job.backend, &job.escl_host, job.escl_port) {
        Ok(b) => b,
        Err(e) => {
            let _ = tx.send(UiEvent::Done(Err(e.message())));
            return;
        }
    };
    let request = ScanRequest {
        device: DeviceId(job.device),
        paper_source: job.source,
        page_size: job.page_size,
        dpi: job.dpi,
        bit_depth: job.bit_depth,
        brightness: 0,
        contrast: 0,
    };
    let mut sink = TxSink { tx: tx.clone() };
    let result = backend.scan(&request, &mut sink, &cancel).map_err(|e| e.message());
    let _ = tx.send(UiEvent::Done(result));
}

/// Scale (w,h) down so the width fits `max_w` (aspect preserved).
fn fit_inside(w: u32, h: u32, max_w: u32) -> Option<(u32, u32)> {
    if w == 0 || h == 0 || max_w == 0 {
        return None;
    }
    if w <= max_w {
        return Some((w, h));
    }
    let h2 = u64::from(h).checked_mul(u64::from(max_w))?.checked_div(u64::from(w))?;
    let h2 = u32::try_from(h2).ok()?;
    if h2 == 0 {
        return None;
    }
    Some((max_w, h2))
}

/// Panic-free RGB8 → egui image (length-checked first).
fn color_image_from_raw(raw: &RawPage) -> Option<egui::ColorImage> {
    let w = usize::try_from(raw.width_px).ok()?;
    let h = usize::try_from(raw.height_px).ok()?;
    let want = w.checked_mul(h)?.checked_mul(3)?;
    if want == 0 || raw.pixels_rgb8.len() != want {
        return None;
    }
    let pixels = raw.pixels_rgb8.as_chunks::<3>().0.iter().map(|p| egui::Color32::from_rgb(p[0], p[1], p[2])).collect();
    Some(egui::ColorImage::new([w, h], pixels))
}

struct PageView {
    id: u64,
    thumb: Option<egui::TextureHandle>,
    full: Option<egui::TextureHandle>,
    w: u32,
    h: u32,
    dpi: u32,
}

fn source_label(source: PaperSource) -> &'static str {
    match source {
        PaperSource::Auto => "Auto",
        PaperSource::Flatbed => "Flatbed",
        PaperSource::Feeder => "Feeder",
        PaperSource::Duplex => "Duplex",
    }
}

fn bit_depth_label(depth: BitDepth) -> &'static str {
    match depth {
        BitDepth::Color => "Color",
        BitDepth::Grayscale => "Grayscale",
        BitDepth::BlackWhite => "Black & white",
    }
}

fn page_size_label(size: PageSize) -> String {
    match size.dims_mm() {
        Ok((w, h)) => {
            let name = match size {
                PageSize::Letter => "Letter",
                PageSize::A4 => "A4",
                PageSize::Legal => "Legal",
                PageSize::A5 => "A5",
                PageSize::A3 => "A3",
                PageSize::B4 => "B4",
                PageSize::B5 => "B5",
                PageSize::Custom { .. } => "Custom",
            };
            format!("{name} ({w:.0}x{h:.0}mm)")
        }
        Err(_) => "Custom".to_owned(),
    }
}

struct CraftApp {
    engine: Engine,
    doc: u64,
    devices: Vec<DeviceInfo>,
    backend_idx: usize,
    device_idx: usize,
    dpi_idx: usize,
    source: PaperSource,
    bit_depth: BitDepth,
    page_size_idx: usize,
    views: Vec<PageView>,
    selected: Option<u64>,
    scanning: bool,
    rx: Option<Receiver<UiEvent>>,
    cancel: Option<CancelToken>,
    status: String,
    export_path: String,
    export_idx: usize,
    export_title: String,
    export_author: String,
    profile_name: String,
    profiles: Vec<StoredProfile>,
    profile_idx: Option<usize>,
    store: ProfileStore,
    escl_host: String,
    escl_port: String,
}

impl CraftApp {
    fn new() -> Self {
        let store = ProfileStore::new(std::path::PathBuf::from("profiles.json"));
        let profiles = store.load().unwrap_or_default();
        let mut engine = Engine::new();
        let doc = engine.create_document();
        // Default to the real platform backend (WIA on Windows), not the mock,
        // and list its devices immediately so Scan works on first launch.
        let backend_idx = if cfg!(target_os = "windows") { BACKENDS.iter().position(|b| *b == "wia").unwrap_or(0) } else { 0 };
        let mut app = Self {
            engine,
            doc,
            devices: Vec::new(),
            backend_idx,
            device_idx: 0,
            dpi_idx: 2,
            source: PaperSource::Flatbed,
            bit_depth: BitDepth::Color,
            page_size_idx: 0,
            views: Vec::new(),
            selected: None,
            scanning: false,
            rx: None,
            cancel: None,
            status: "Press Refresh to list scanners, then Scan.".to_owned(),
            export_path: "scan.pdf".to_owned(),
            export_idx: 0,
            export_title: String::new(),
            export_author: String::new(),
            profile_name: String::new(),
            profiles,
            profile_idx: None,
            store,
            escl_host: "192.168.1.10".to_owned(),
            escl_port: "80".to_owned(),
        };
        app.refresh_devices();
        app
    }

    fn backend_name(&self) -> &str {
        BACKENDS.get(self.backend_idx).copied().unwrap_or("mock")
    }

    fn current_dpi(&self) -> u32 {
        DPIS.get(self.dpi_idx).copied().unwrap_or(200)
    }

    fn current_page_size(&self) -> PageSize {
        PAGE_SIZES.get(self.page_size_idx).copied().unwrap_or(PageSize::Letter)
    }

    fn selected_device_id(&self) -> Option<String> {
        self.devices.get(self.device_idx).map(|d| d.id.0.clone())
    }

    fn refresh_devices(&mut self) {
        let name = self.backend_name().to_owned();
        let backend = make_backend(&name, &self.escl_host, self.parse_escl_port());
        match backend {
            Err(e) => {
                self.devices.clear();
                self.status = e.message();
            }
            Ok(b) => match b.list_devices(&CancelToken::new()) {
                Err(e) => {
                    self.devices.clear();
                    self.status = e.message();
                }
                Ok(devices) => {
                    self.device_idx = 0;
                    if devices.is_empty() {
                        self.status = format!("No {} scanners found.", self.backend_name());
                    } else {
                        self.status = format!("Found {} scanner(s).", devices.len());
                    }
                    self.devices = devices;
                }
            },
        }
    }

    fn parse_escl_port(&self) -> u16 {
        self.escl_port.parse::<u16>().unwrap_or(80)
    }

    fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        let Some(device) = self.selected_device_id() else {
            self.status = "No device selected. Press Refresh first.".to_owned();
            return;
        };
        let job = ScanJob {
            backend: self.backend_name().to_owned(),
            device,
            dpi: self.current_dpi(),
            source: self.source,
            bit_depth: self.bit_depth,
            page_size: self.current_page_size(),
            escl_host: self.escl_host.clone(),
            escl_port: self.parse_escl_port(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        std::thread::spawn(move || run_scan_job(job, tx, worker_cancel));
        self.rx = Some(rx);
        self.cancel = Some(cancel);
        self.scanning = true;
        self.status = "Scanning…".to_owned();
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        let events: Vec<UiEvent> = match &self.rx {
            Some(rx) => {
                let mut out = Vec::new();
                while let Ok(event) = rx.try_recv() {
                    out.push(event);
                    if out.len() >= 64 {
                        break;
                    }
                }
                out
            }
            None => Vec::new(),
        };
        for event in events {
            match event {
                UiEvent::Page(raw) => self.add_page(ctx, raw),
                UiEvent::Progress(n) => {
                    self.status = format!("Scanning… {n} page(s)");
                }
                UiEvent::Done(result) => {
                    self.scanning = false;
                    self.rx = None;
                    self.cancel = None;
                    match result {
                        Ok(n) => {
                            self.status = format!("Scan complete: {n} page(s).");
                        }
                        Err(message) => {
                            self.status = message;
                        }
                    }
                }
            }
        }
    }

    fn add_page(&mut self, ctx: &egui::Context, raw: RawPage) {
        let w = raw.width_px;
        let h = raw.height_px;
        let dpi = raw.dpi;
        let id = match self.engine.document_mut(self.doc) {
            Some(doc) => match doc.append_page(raw.clone()) {
                Ok(id) => id,
                Err(e) => {
                    self.status = e.message();
                    return;
                }
            },
            None => {
                self.status = "Document went away.".to_owned();
                return;
            }
        };
        let thumb = self.make_texture(ctx, id, "thumb", &raw, THUMB_W);
        if thumb.is_none() {
            self.status = "Scanned a page the previewer cannot show.".to_owned();
        }
        self.views.push(PageView { id, thumb, full: None, w, h, dpi });
        self.selected = Some(id);
    }

    fn make_texture(&self, ctx: &egui::Context, id: u64, kind: &str, raw: &RawPage, max_w: u32) -> Option<egui::TextureHandle> {
        let (w, h) = fit_inside(raw.width_px, raw.height_px, max_w)?;
        let mut small = raw.clone();
        if w != raw.width_px || h != raw.height_px {
            scancraft_imaging::apply_transform(&mut small, &Transform::Resize { w, h }).ok()?;
            if small.width_px != w || small.height_px != h || small.pixels_rgb8.is_empty() {
                return None;
            }
        }
        let image = color_image_from_raw(&small)?;
        Some(ctx.load_texture(format!("page-{id}-{kind}"), image, egui::TextureOptions::LINEAR))
    }

    fn rebuild_view(&mut self, ctx: &egui::Context, id: u64) {
        let raw = self.engine.document(self.doc).and_then(|doc| doc.pages().iter().find(|p| p.id == id).map(|p| p.raw.clone()));
        let Some(raw) = raw else { return };
        let thumb = self.make_texture(ctx, id, "thumb", &raw, THUMB_W);
        if let Some(view) = self.views.iter_mut().find(|v| v.id == id) {
            view.thumb = thumb;
            view.full = None;
            view.w = raw.width_px;
            view.h = raw.height_px;
            view.dpi = raw.dpi;
        }
    }

    fn rebuild_all_views(&mut self, ctx: &egui::Context) {
        let raws: Vec<(u64, RawPage)> = match self.engine.document(self.doc) {
            Some(doc) => doc.pages().iter().map(|p| (p.id, p.raw.clone())).collect(),
            None => Vec::new(),
        };
        self.views.clear();
        for (id, raw) in raws {
            let thumb = self.make_texture(ctx, id, "thumb", &raw, THUMB_W);
            self.views.push(PageView { id, thumb, full: None, w: raw.width_px, h: raw.height_px, dpi: raw.dpi });
        }
        if let Some(selected) = self.selected
            && !self.views.iter().any(|v| v.id == selected)
        {
            self.selected = self.views.first().map(|v| v.id);
        }
    }

    fn ensure_full(&mut self, ctx: &egui::Context, id: u64) {
        let needs = self.views.iter().any(|v| v.id == id && v.full.is_none());
        if !needs {
            return;
        }
        let raw = self.engine.document(self.doc).and_then(|doc| doc.pages().iter().find(|p| p.id == id).map(|p| p.raw.clone()));
        let Some(raw) = raw else { return };
        let full = self.make_texture(ctx, id, "full", &raw, PREVIEW_W);
        if let Some(view) = self.views.iter_mut().find(|v| v.id == id) {
            view.full = full;
        }
    }

    fn do_export(&mut self) {
        if self.export_path.trim().is_empty() {
            self.status = "Choose an output file first.".to_owned();
            return;
        }
        let format = match self.export_idx {
            1 => ExportFormat::Tiff,
            2 => ExportFormat::Png,
            3 => ExportFormat::Jpeg,
            4 => ExportFormat::Bmp,
            _ => ExportFormat::Pdf,
        };
        let raws: Vec<RawPage> = match self.engine.document(self.doc) {
            Some(doc) => doc.pages().iter().map(|p| p.raw.clone()).collect(),
            None => Vec::new(),
        };
        if raws.is_empty() {
            self.status = "Nothing to export: scan some pages first.".to_owned();
            return;
        }
        let meta = PdfMetadata { title: self.export_title.clone(), author: self.export_author.clone() };
        let bytes = match export_pages(&raws, format, &meta, 85) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.status = e.message();
                return;
            }
        };
        let path = std::path::PathBuf::from(&self.export_path);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.status = format!("Cannot create output folder: {e}");
            return;
        }
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.status = format!("Saved {} page(s) to {}.", raws.len(), self.export_path);
            }
            Err(e) => {
                self.status = format!("Cannot write output file: {e}");
            }
        }
    }

    fn save_profile(&mut self) {
        if self.profile_name.trim().is_empty() {
            self.status = "Name the profile first.".to_owned();
            return;
        }
        let profile = scancraft_model::ScanProfile {
            display_name: self.profile_name.clone(),
            driver: scancraft_model::Driver::parse_name(self.backend_name()),
            device_id: self.selected_device_id().unwrap_or_default(),
            paper_source: match self.source {
                PaperSource::Flatbed | PaperSource::Auto => ScanSource::Glass,
                PaperSource::Feeder => ScanSource::Feeder,
                PaperSource::Duplex => ScanSource::Duplex,
            },
            dpi: self.current_dpi(),
            page_size: self.current_page_size(),
            bit_depth: self.bit_depth,
            page_align: scancraft_model::HorizontalAlign::Right,
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
        };
        let stored = StoredProfile { name: self.profile_name.clone(), profile, is_default: false };
        match self.store.add(stored) {
            Ok(()) => {
                self.profiles = self.store.load().unwrap_or_default();
                self.status = format!("Saved profile '{}'.", self.profile_name);
            }
            Err(e) => {
                self.status = e.message();
            }
        }
    }

    fn apply_profile(&mut self, index: usize) {
        let Some(stored) = self.profiles.get(index) else {
            return;
        };
        let profile = stored.profile.clone();
        let stored_name = stored.name.clone();
        let stored_device = profile.device_id.clone();
        for (i, name) in BACKENDS.iter().enumerate() {
            if profile.driver == scancraft_model::Driver::parse_name(name) {
                self.backend_idx = i;
            }
        }
        self.source = profile.paper_source.to_paper_source();
        self.bit_depth = profile.bit_depth;
        for (i, size) in PAGE_SIZES.iter().enumerate() {
            if *size == profile.page_size {
                self.page_size_idx = i;
            }
        }
        for (i, dpi) in DPIS.iter().enumerate() {
            if *dpi == profile.dpi {
                self.dpi_idx = i;
            }
        }
        self.profile_name = stored_name.clone();
        self.profile_idx = Some(index);
        self.refresh_devices();
        for (i, device) in self.devices.iter().enumerate() {
            if device.id.0 == stored_device {
                self.device_idx = i;
            }
        }
        self.status = format!("Loaded profile '{stored_name}'.");
    }

    fn show_left(&mut self, ui: &mut egui::Ui) {
        ui.heading("ScanCraft");
        ui.separator();
        ui.label("Backend");
        egui::ComboBox::from_label("").selected_text(self.backend_name()).show_ui(ui, |ui| {
            for (i, name) in BACKENDS.iter().enumerate() {
                ui.selectable_value(&mut self.backend_idx, i, *name);
            }
        });
        if self.backend_name() == "escl" {
            ui.horizontal(|ui| {
                ui.label("Host");
                ui.text_edit_singleline(&mut self.escl_host);
            });
            ui.horizontal(|ui| {
                ui.label("Port");
                ui.text_edit_singleline(&mut self.escl_port);
            });
        }
        ui.horizontal(|ui| {
            if ui.button("Refresh devices").clicked() {
                self.refresh_devices();
            }
        });
        ui.label("Device");
        let current = self.devices.get(self.device_idx).map(|d| d.display_name.clone()).unwrap_or_else(|| "—".to_owned());
        egui::ComboBox::from_label("").selected_text(current).show_ui(ui, |ui| {
            for (i, device) in self.devices.iter().enumerate() {
                ui.selectable_value(&mut self.device_idx, i, &device.display_name);
            }
        });
        ui.separator();
        ui.label("Paper source");
        egui::ComboBox::from_label("").selected_text(source_label(self.source)).show_ui(ui, |ui| {
            ui.selectable_value(&mut self.source, PaperSource::Flatbed, "Flatbed");
            ui.selectable_value(&mut self.source, PaperSource::Feeder, "Feeder");
            ui.selectable_value(&mut self.source, PaperSource::Duplex, "Duplex");
        });
        ui.label("Color mode");
        egui::ComboBox::from_label("").selected_text(bit_depth_label(self.bit_depth)).show_ui(ui, |ui| {
            ui.selectable_value(&mut self.bit_depth, BitDepth::Color, "Color");
            ui.selectable_value(&mut self.bit_depth, BitDepth::Grayscale, "Grayscale");
            ui.selectable_value(&mut self.bit_depth, BitDepth::BlackWhite, "Black & white");
        });
        ui.label("Page size");
        let size_text = page_size_label(self.current_page_size());
        egui::ComboBox::from_label("").selected_text(size_text).show_ui(ui, |ui| {
            for (i, size) in PAGE_SIZES.iter().enumerate() {
                ui.selectable_value(&mut self.page_size_idx, i, page_size_label(*size));
            }
        });
        ui.label(format!("Resolution: {} dpi", self.current_dpi()));
        ui.add(egui::Slider::new(&mut self.dpi_idx, 0..=DPIS.len().saturating_sub(1)).show_value(false));
        ui.separator();
        ui.horizontal(|ui| {
            let scan = ui.add_enabled(!self.scanning, egui::Button::new("Scan"));
            if scan.clicked() {
                self.start_scan();
            }
            if self.scanning {
                let cancel = ui.button("Cancel");
                if cancel.clicked() {
                    if let Some(token) = &self.cancel {
                        token.cancel();
                    }
                    self.status = "Cancelling…".to_owned();
                }
            }
        });
        ui.separator();
        ui.label("Profiles");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.profile_name);
            if ui.button("Save").clicked() {
                self.save_profile();
            }
        });
        let profile_names: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();
        let profile_current = self.profile_idx;
        let selected_name = profile_current.and_then(|i| profile_names.get(i)).cloned().unwrap_or_else(|| "—".to_owned());
        egui::ComboBox::from_label("").selected_text(selected_name).show_ui(ui, |ui| {
            for (i, name) in profile_names.iter().enumerate() {
                if ui.selectable_label(profile_current == Some(i), name).clicked() {
                    self.apply_profile(i);
                }
            }
        });
    }

    fn show_center(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(format!("Pages ({})", self.views.len()));
            let remove = ui.add_enabled(self.selected.is_some(), egui::Button::new("Remove"));
            if remove.clicked()
                && let Some(id) = self.selected
                && let Some(doc) = self.engine.document_mut(self.doc)
            {
                match doc.remove_page(id) {
                    Ok(()) => {
                        self.views.retain(|v| v.id != id);
                        self.selected = self.views.first().map(|v| v.id);
                        self.status = "Page removed (Undo restores it).".to_owned();
                    }
                    Err(e) => {
                        self.status = e.message();
                    }
                }
            }
            if ui.button("Undo").clicked() {
                if let Some(doc) = self.engine.document_mut(self.doc) {
                    if doc.undo() {
                        self.status = "Undone.".to_owned();
                    } else {
                        self.status = "Nothing to undo.".to_owned();
                    }
                }
                self.rebuild_all_views(ctx);
            }
            let rotate = ui.add_enabled(self.selected.is_some(), egui::Button::new("Rotate 90°"));
            if rotate.clicked()
                && let Some(id) = self.selected
            {
                if let Some(doc) = self.engine.document_mut(self.doc) {
                    match doc.transform_page(id, &Transform::Rotate90) {
                        Ok(()) => {
                            self.status = "Rotated.".to_owned();
                        }
                        Err(e) => {
                            self.status = e.message();
                        }
                    }
                }
                self.rebuild_view(ctx, id);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let ids: Vec<u64> = self.views.iter().map(|v| v.id).collect();
                for id in ids {
                    let (marked, thumb) =
                        self.views.iter().find(|v| v.id == id).map(|v| (Some(v.id) == self.selected, v.thumb.clone())).unwrap_or((false, None));
                    ui.vertical(|ui| {
                        let prefix = if marked { "▶ " } else { "" };
                        if let Some(tex) = thumb {
                            let response = ui.add(egui::Image::new(&tex).sense(egui::Sense::click()));
                            if response.clicked() {
                                self.selected = Some(id);
                            }
                            ui.label(format!("{prefix}page {id}"));
                        } else {
                            if ui.button(format!("{prefix}page {id} (no preview)")).clicked() {
                                self.selected = Some(id);
                            }
                        }
                    });
                }
                if self.views.is_empty() {
                    ui.label("No pages yet. Configure the scanner on the left and press Scan.");
                }
            });
        });
    }

    fn show_right(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.heading("Page");
        if let Some(id) = self.selected {
            self.ensure_full(ctx, id);
            let full = self.views.iter().find(|v| v.id == id).and_then(|v| v.full.clone());
            let info = self.views.iter().find(|v| v.id == id).map(|v| (v.w, v.h, v.dpi));
            if let Some((w, h, dpi)) = info {
                ui.label(format!("{w}×{h}px at {dpi} dpi"));
            }
            if let Some(tex) = full {
                egui::ScrollArea::both().show(ui, |ui| {
                    ui.image(&tex);
                });
            } else {
                ui.label("Preview unavailable for this page.");
            }
        } else {
            ui.label("Select a page to preview it.");
        }
        ui.separator();
        ui.heading("Export");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.export_path);
            if ui.button("Browse…").clicked() {
                let dialog = rfd::FileDialog::new().set_file_name("scan.pdf");
                let dialog = match self.export_idx {
                    1 => dialog.add_filter("TIFF image", &["tif", "tiff"]),
                    2 => dialog.add_filter("PNG image", &["png"]),
                    3 => dialog.add_filter("JPEG image", &["jpg", "jpeg"]),
                    4 => dialog.add_filter("BMP image", &["bmp"]),
                    _ => dialog.add_filter("PDF document", &["pdf"]),
                };
                if let Some(path) = dialog.save_file() {
                    self.export_path = path.to_string_lossy().to_string();
                }
            }
        });
        egui::ComboBox::from_label("Format").selected_text(EXPORT_NAMES.get(self.export_idx).copied().unwrap_or("PDF")).show_ui(ui, |ui| {
            for (i, name) in EXPORT_NAMES.iter().enumerate() {
                ui.selectable_value(&mut self.export_idx, i, *name);
            }
        });
        if self.export_idx == 0 {
            ui.horizontal(|ui| {
                ui.label("Title");
                ui.text_edit_singleline(&mut self.export_title);
            });
            ui.horizontal(|ui| {
                ui.label("Author");
                ui.text_edit_singleline(&mut self.export_author);
            });
        }
        if ui.button("Export").clicked() {
            self.do_export();
        }
        if !self.export_path.trim().is_empty() {
            let ext = self.export_path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            if format_from_extension(&ext).is_none() {
                ui.label("Tip: use .pdf, .tiff, .png, .jpg, or .bmp.");
            }
        }
    }
}

impl eframe::App for CraftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        self.poll_events(&ctx);
        if self.scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("ScanCraft");
                ui.label("scan → organize → export");
            });
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.label(&self.status);
        });
        egui::Panel::left("left").default_size(280.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.show_left(ui);
            });
        });
        egui::Panel::right("right").default_size(320.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.show_right(&ctx, ui);
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.show_center(&ctx, ui);
        });
    }
}

fn main() {
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 760.0]), ..Default::default() };
    let result = eframe::run_native("ScanCraft", options, Box::new(|_cc| Ok(Box::new(CraftApp::new()) as Box<dyn eframe::App>)));
    if let Err(err) = result {
        eprintln!("ScanCraft failed to start: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32) -> RawPage {
        RawPage {
            width_px: w,
            height_px: h,
            dpi: 200,
            bit_depth: BitDepth::Color,
            side: scancraft_model::PageSide::Front,
            pixels_rgb8: vec![128u8; usize::try_from(w).unwrap_or(0) * usize::try_from(h).unwrap_or(0) * 3],
        }
    }

    #[test]
    fn fit_inside_downscales_wide_pages() {
        assert_eq!(fit_inside(1000, 500, 250), Some((250, 125)));
        assert_eq!(fit_inside(200, 100, 300), Some((200, 100)));
        assert_eq!(fit_inside(0, 10, 300), None);
        assert_eq!(fit_inside(10, 10, 0), None);
    }

    #[test]
    fn color_image_conversion_checks_length() {
        let good = solid(4, 2);
        let image = color_image_from_raw(&good).unwrap();
        assert_eq!(image.size, [4, 2]);
        assert_eq!(image.pixels.len(), 8);
        let mut bad = solid(4, 2);
        bad.pixels_rgb8.pop();
        assert!(color_image_from_raw(&bad).is_none());
    }

    #[test]
    fn backend_names_are_known() {
        assert!(BACKENDS.contains(&"mock"));
        assert!(BACKENDS.contains(&"wia"));
        assert!(make_backend("nope", "", 80).is_err());
    }
}
