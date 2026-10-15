//! Headless ScanCraft CLI. Today: `devices` and a mock `scan` demo.
//! The full NAPS2-style tool table (profiles, PDF export, OCR) lands later.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use scancraft_engine::Engine;
use scancraft_model::{PageSize, PaperSource};
use scancraft_scan_core::ScannerBackend;
use scancraft_scan_core::{CancelToken, DeviceId, PageSink, Progress, RawPage, ScanError, ScanRequest};
use scancraft_scan_mock::MockScanner;

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
    "usage:\n  scancraft-cli devices\n  scancraft-cli scan [--device <id>] [--dpi <n>]"
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
        "devices" => {
            let mock = MockScanner::new();
            let devs = mock.list_devices(&CancelToken::new())?;
            for d in devs {
                println!("{}\t{}", d.id.0, d.display_name);
            }
            Ok(())
        }
        "scan" => {
            let mut device = "mock:feeder".to_owned();
            let mut dpi: u32 = 200;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--device" {
                    match args.get(i + 1) {
                        Some(v) => device = v.clone(),
                        None => return Err(ScanError::Unsupported("missing --device value".to_owned())),
                    }
                    i += 2;
                } else if args[i] == "--dpi" {
                    match args.get(i + 1) {
                        Some(v) => match v.parse::<u32>() {
                            Ok(n) => dpi = n,
                            Err(_) => return Err(ScanError::Unsupported("bad --dpi value".to_owned())),
                        },
                        None => return Err(ScanError::Unsupported("missing --dpi value".to_owned())),
                    }
                    i += 2;
                } else {
                    return Err(ScanError::Unsupported(format!("unknown flag: {}", args[i])));
                }
            }
            if !(50..=4800).contains(&dpi) {
                return Err(ScanError::Unsupported("dpi must be between 50 and 4800".to_owned()));
            }
            let mock = MockScanner::new();
            let req = ScanRequest {
                device: DeviceId(device),
                paper_source: PaperSource::Feeder,
                page_size: PageSize::Letter,
                dpi,
                bit_depth: scancraft_model::BitDepth::Color,
                brightness: 0,
                contrast: 0,
            };
            let mut engine = Engine::new();
            let doc = engine.create_document();
            let mut sink = EngineSink { engine: &mut engine, doc };
            let n = mock.scan(&req, &mut sink, &CancelToken::new())?;
            let count = match sink.engine.document(doc) {
                Some(d) => d.pages().len(),
                None => 0,
            };
            println!("scanned {n} pages into document {doc} ({count} stored)");
            Ok(())
        }
        _ => {
            eprintln!("{usage}", usage = usage());
            Err(ScanError::Unsupported("unknown command".to_owned()))
        }
    }
}
