//! Persisted scan profiles store (JSON file).
//!
//! Port of NAPS2's profile persistence (`NAPS2.Lib/Scan/ScanProfile.cs` plus
//! the XML file store in `naps2-in c++-master #For Reference/`); logic
//! ported, never bulk-copied. This project is GPL-2.0-or-later like NAPS2.
//!
//! Format is JSON (not NAPS2's XML) on purpose: one file holding a list of
//! [`StoredProfile`]. Missing file reads as empty; corrupt JSON is
//! [`ScanError::Io`]. Writes are atomic-ish (temp file + rename).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::PathBuf;

use scancraft_model::ScanProfile;
use scancraft_scan_core::ScanError;
use serde::{Deserialize, Serialize};

/// One persisted profile: user-visible name plus the [`ScanProfile`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredProfile {
    pub name: String,
    pub profile: ScanProfile,
    pub is_default: bool,
}

/// JSON-file-backed profile store.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    pub path: PathBuf,
}

impl ProfileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Load all profiles. Missing file (or blank file) yields empty.
    /// Corrupt JSON yields [`ScanError::Io`].
    pub fn load(&self) -> Result<Vec<StoredProfile>, ScanError> {
        match std::fs::read_to_string(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(ScanError::Io(cap_msg(e.to_string()))),
            Ok(text) => {
                if text.trim().is_empty() {
                    Ok(Vec::new())
                } else {
                    serde_json::from_str::<Vec<StoredProfile>>(&text).map_err(|e| ScanError::Io(cap_msg(e.to_string())))
                }
            }
        }
    }

    /// Save all profiles, creating parent dirs. Atomic-ish: write temp file
    /// in the same directory, then rename over the target.
    pub fn save_all(&self, profiles: &[StoredProfile]) -> Result<(), ScanError> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| ScanError::Io(cap_msg(e.to_string())))?;
        }
        let json = serde_json::to_string_pretty(profiles).map_err(|e| ScanError::Io(cap_msg(e.to_string())))?;
        let mut tmp_os = self.path.clone().into_os_string();
        tmp_os.push(format!(".tmp-{}", std::process::id()));
        let tmp = PathBuf::from(tmp_os);
        let write_result = std::fs::write(&tmp, json).map_err(|e| ScanError::Io(cap_msg(e.to_string())));
        if let Err(e) = write_result {
            let _ignored = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ignored = std::fs::remove_file(&tmp);
            return Err(ScanError::Io(cap_msg(e.to_string())));
        }
        Ok(())
    }

    /// Add or replace the profile with the same name.
    pub fn add(&self, stored: StoredProfile) -> Result<(), ScanError> {
        let mut all = self.load()?;
        all.retain(|p| p.name != stored.name);
        all.push(stored);
        self.save_all(&all)
    }

    /// Remove the profile with `name`. Returns whether one was removed.
    pub fn remove(&self, name: &str) -> Result<bool, ScanError> {
        let all = self.load()?;
        let before = all.len();
        let kept: Vec<StoredProfile> = all.into_iter().filter(|p| p.name != name).collect();
        if kept.len() == before {
            return Ok(false);
        }
        self.save_all(&kept)?;
        Ok(true)
    }
}

fn cap_msg(s: String) -> String {
    const CAP: usize = 300;
    if s.len() <= CAP { s } else { s.chars().take(CAP).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("scancraft-profiles-test-{}-{tag}", std::process::id()));
        p
    }

    fn store_in(tag: &str) -> (ProfileStore, PathBuf) {
        let dir = unique_dir(tag);
        let _ = std::fs::create_dir_all(&dir);
        let mut path = dir.clone();
        path.push("profiles.json");
        (ProfileStore::new(path), dir)
    }

    fn sample(name: &str) -> StoredProfile {
        StoredProfile { name: name.to_owned(), profile: ScanProfile::default(), is_default: false }
    }

    #[test]
    fn roundtrip_save_load_add_remove() {
        let (store, dir) = store_in("roundtrip");
        let _ = std::fs::remove_file(&store.path);
        assert!(store.save_all(&[sample("a"), sample("b")]).is_ok());
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 2);

        store.add(sample("c")).unwrap();
        assert_eq!(store.load().unwrap().len(), 3);
        // Re-adding the same name replaces instead of duplicating.
        store.add(sample("c")).unwrap();
        assert_eq!(store.load().unwrap().len(), 3);

        assert!(store.remove("a").unwrap());
        assert!(!store.remove("missing").unwrap());
        let names: Vec<String> = store.load().unwrap().into_iter().map(|p| p.name).collect();
        assert!(names.contains(&"b".to_owned()));
        assert!(!names.contains(&"a".to_owned()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_empty() {
        let (store, dir) = store_in("missing");
        let _ = std::fs::remove_file(&store.path);
        let loaded = store.load().unwrap();
        assert!(loaded.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_io_error() {
        let (store, dir) = store_in("corrupt");
        let _ = std::fs::create_dir_all(store.path.parent().unwrap());
        std::fs::write(&store.path, "{ not json !!!").unwrap();
        let err = store.load().unwrap_err();
        assert!(matches!(err, ScanError::Io(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_parent_dirs() {
        let mut dir = unique_dir("parents");
        dir.push("nested");
        dir.push("deep");
        let mut path = dir.clone();
        path.push("profiles.json");
        let _ = std::fs::remove_dir_all(unique_dir("parents"));
        let store = ProfileStore::new(path);
        store.save_all(&[sample("x")]).unwrap();
        assert_eq!(store.load().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(unique_dir("parents"));
    }
}
