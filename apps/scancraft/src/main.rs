//! ScanCraft desktop shell. The egui GUI (PdfCraft look: tokens, palette,
//! thumbnail grid) lands once the scan core matures; today this binary just
//! proves the workspace builds a desktop target for CI (`exe.yml`).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

fn main() {
    println!("scancraft desktop shell v{} — GUI coming soon", env!("CARGO_PKG_VERSION"));
}
