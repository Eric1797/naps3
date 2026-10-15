//! Session/document store. The only frontend entry point for scan data:
//! backends deliver [`RawPage`]s, the engine owns them as [`Page`]s with
//! undo. No I/O here; persistence/export lands in later milestones.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use scancraft_scan_core::{RawPage, ScanError};

/// A stored page with a stable id inside its document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub id: u64,
    pub raw: RawPage,
}

/// Undoable edit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UndoOp {
    Append { doc: u64, page_id: u64 },
    Remove { doc: u64, index: usize, page: Page },
}

/// One open document: ordered pages plus an undo stack.
#[derive(Debug, Default)]
pub struct Document {
    id: u64,
    pages: Vec<Page>,
    undo: Vec<UndoOp>,
    next_page_id: u64,
}

impl Document {
    fn new(id: u64) -> Self {
        Self { id, pages: Vec::new(), undo: Vec::new(), next_page_id: 1 }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    pub fn append_page(&mut self, raw: RawPage) -> Result<u64, ScanError> {
        raw.validate()?;
        let id = self.next_page_id;
        self.next_page_id += 1;
        self.pages.push(Page { id, raw });
        self.undo.push(UndoOp::Append { doc: self.id, page_id: id });
        Ok(id)
    }

    pub fn remove_page(&mut self, page_id: u64) -> Result<(), ScanError> {
        let index = self.pages.iter().position(|p| p.id == page_id).ok_or_else(|| ScanError::Unsupported("page not found".to_owned()))?;
        let page = self.pages.remove(index);
        self.undo.push(UndoOp::Remove { doc: self.id, index, page });
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        let op = self.undo.pop();
        match op {
            None => false,
            Some(UndoOp::Append { page_id, .. }) => {
                if let Some(i) = self.pages.iter().position(|p| p.id == page_id) {
                    self.pages.remove(i);
                }
                true
            }
            Some(UndoOp::Remove { index, page, .. }) => {
                let at = index.min(self.pages.len());
                self.pages.insert(at, page);
                true
            }
        }
    }
}

/// All open documents. Thin on purpose: sessions/profiles/autosave come later.
#[derive(Debug, Default)]
pub struct Engine {
    docs: Vec<Document>,
    next_doc_id: u64,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_document(&mut self) -> u64 {
        self.next_doc_id += 1;
        let id = self.next_doc_id;
        self.docs.push(Document::new(id));
        id
    }

    pub fn document(&self, id: u64) -> Option<&Document> {
        self.docs.iter().find(|d| d.id() == id)
    }

    pub fn document_mut(&mut self, id: u64) -> Option<&mut Document> {
        self.docs.iter_mut().find(|d| d.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scancraft_model::{BitDepth, PageSide};

    fn raw() -> RawPage {
        RawPage { width_px: 4, height_px: 4, dpi: 200, bit_depth: BitDepth::Color, side: PageSide::Front, pixels_rgb8: vec![0u8; 4 * 4 * 3] }
    }

    #[test]
    fn append_then_undo() {
        let mut eng = Engine::new();
        let doc = eng.create_document();
        let doc = eng.document_mut(doc).unwrap();
        let id = doc.append_page(raw()).unwrap();
        assert_eq!(doc.pages().len(), 1);
        assert!(doc.undo());
        assert!(doc.pages().is_empty());
        let _ = id;
    }

    #[test]
    fn remove_then_undo_restores_order() {
        let mut eng = Engine::new();
        let doc = eng.create_document();
        let doc = eng.document_mut(doc).unwrap();
        let a = doc.append_page(raw()).unwrap();
        let _ = doc.append_page(raw()).unwrap();
        doc.remove_page(a).unwrap();
        assert_eq!(doc.pages().len(), 1);
        assert!(doc.undo());
        assert_eq!(doc.pages().len(), 2);
        assert_eq!(doc.pages()[0].id, a);
    }
}
