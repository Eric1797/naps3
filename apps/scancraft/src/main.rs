//! ScanCraft desktop shell. The egui GUI (PdfCraft look: tokens, palette,
//! thumbnail grid) lands once the scan core matures; today this binary just
//! proves the workspace builds a desktop target for CI (`exe.yml`).
//!
//! Port of the NAPS2 desktop entry surface; logic ported, never bulk-copied.
//! This project is GPL-2.0-or-later like NAPS2.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

fn main() {
    println!("scancraft desktop shell v{} — GUI coming soon", env!("CARGO_PKG_VERSION"));
    println!("backends: mock, wia (windows), twain, sane, escl");
    println!("GUI milestone pending");
}
