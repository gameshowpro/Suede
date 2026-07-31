//! Storage for uploaded wallpapers.
//!
//! A wallpaper gives an appliance something to show when no window is open —
//! during a browser restart, or before the first app launches. Sway draws it
//! through `swaybg`, so the file has to exist on disk for the compositor to
//! read; it cannot be held in memory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Largest image accepted, so an upload cannot fill the state partition.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WallpaperError {
    #[error("wallpaper id {0:?} may only contain letters, digits, '-', '_' and '.'")]
    InvalidId(String),
    #[error("no wallpaper named {0}")]
    NotFound(String),
    #[error("unrecognised image format; PNG and JPEG are supported")]
    UnsupportedFormat,
    #[error("image is {size} bytes, over the {MAX_BYTES} byte limit")]
    TooLarge { size: usize },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// An image format Suede will accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

/// Identify an image from its leading bytes.
///
/// Sniffed rather than taken from the request, because a caller that mislabels
/// its upload would otherwise leave swaybg failing with nothing to explain it.
pub fn detect_format(bytes: &[u8]) -> Option<Format> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(PNG) {
        return Some(Format::Png);
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Format::Jpeg);
    }
    None
}

/// Metadata for a stored wallpaper, as served by the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Wallpaper {
    /// Client-chosen identifier, referenced from an output's background.
    pub id: String,
    /// `image/png` or `image/jpeg`.
    pub content_type: String,
    pub bytes: u64,
    /// Unix seconds when it was stored.
    pub uploaded_at: u64,
}

pub struct WallpaperStore {
    dir: PathBuf,
}

impl WallpaperStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }

    /// Ids share the app-id rules: they become file names.
    fn validate_id(id: &str) -> Result<(), WallpaperError> {
        let acceptable = !id.is_empty()
            && id != "."
            && !id.contains("..")
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        if acceptable {
            Ok(())
        } else {
            Err(WallpaperError::InvalidId(id.to_string()))
        }
    }

    fn path_for(&self, id: &str, format: Format) -> PathBuf {
        self.dir.join(format!("{id}.{}", format.extension()))
    }

    /// The stored file for `id`, whatever format it was saved in.
    pub fn resolve(&self, id: &str) -> Result<PathBuf, WallpaperError> {
        Self::validate_id(id)?;
        for format in [Format::Png, Format::Jpeg] {
            let path = self.path_for(id, format);
            if path.exists() {
                return Ok(path);
            }
        }
        Err(WallpaperError::NotFound(id.to_string()))
    }

    /// Store an image, replacing any existing wallpaper with the same id.
    pub fn store(&self, id: &str, bytes: &[u8]) -> Result<Wallpaper, WallpaperError> {
        Self::validate_id(id)?;
        if bytes.len() > MAX_BYTES {
            return Err(WallpaperError::TooLarge { size: bytes.len() });
        }
        let format = detect_format(bytes).ok_or(WallpaperError::UnsupportedFormat)?;

        std::fs::create_dir_all(&self.dir)?;
        // Replacing a wallpaper stored in the other format would otherwise
        // leave both on disk, and `resolve` would keep finding the stale one.
        for other in [Format::Png, Format::Jpeg] {
            if other != format {
                let _ = std::fs::remove_file(self.path_for(id, other));
            }
        }
        std::fs::write(self.path_for(id, format), bytes)?;

        Ok(Wallpaper {
            id: id.to_string(),
            content_type: format.content_type().to_string(),
            bytes: bytes.len() as u64,
            uploaded_at: crate::util::unix_now(),
        })
    }

    pub fn read(&self, id: &str) -> Result<(Vec<u8>, Format), WallpaperError> {
        let path = self.resolve(id)?;
        let format =
            detect_format(&std::fs::read(&path)?).ok_or(WallpaperError::UnsupportedFormat)?;
        Ok((std::fs::read(&path)?, format))
    }

    pub fn remove(&self, id: &str) -> Result<(), WallpaperError> {
        let path = self.resolve(id)?;
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// Every stored wallpaper, ordered by id.
    pub fn list(&self) -> Vec<Wallpaper> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut wallpapers: Vec<Wallpaper> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let stem = path.file_stem()?.to_str()?.to_string();
                let metadata = entry.metadata().ok()?;
                let format = match path.extension()?.to_str()? {
                    "png" => Format::Png,
                    "jpg" => Format::Jpeg,
                    _ => return None,
                };
                Some(Wallpaper {
                    id: stem,
                    content_type: format.content_type().to_string(),
                    bytes: metadata.len(),
                    uploaded_at: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                })
            })
            .collect();
        wallpapers.sort_by(|a, b| a.id.cmp(&b.id));
        wallpapers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest valid PNG: an 1x1 image.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];

    fn store() -> (WallpaperStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (WallpaperStore::new(dir.path().join("wallpapers")), dir)
    }

    #[test]
    fn detects_the_formats_it_accepts() {
        assert_eq!(detect_format(PNG), Some(Format::Png));
        assert_eq!(detect_format(JPEG), Some(Format::Jpeg));
        assert_eq!(detect_format(b"GIF89a"), None);
        assert_eq!(detect_format(b""), None);
    }

    #[test]
    fn stores_and_reads_back() {
        let (store, _dir) = store();
        let meta = store.store("lobby", PNG).unwrap();
        assert_eq!(meta.id, "lobby");
        assert_eq!(meta.content_type, "image/png");
        assert_eq!(meta.bytes, PNG.len() as u64);

        let (bytes, format) = store.read("lobby").unwrap();
        assert_eq!(bytes, PNG);
        assert_eq!(format, Format::Png);
    }

    #[test]
    fn a_mislabelled_upload_is_refused() {
        // Sniffed from the content, so a caller cannot store something swaybg
        // would fail to draw with no explanation.
        let (store, _dir) = store();
        let error = store.store("bad", b"not an image at all").unwrap_err();
        assert!(matches!(error, WallpaperError::UnsupportedFormat));
        assert!(store.resolve("bad").is_err(), "nothing should be written");
    }

    #[test]
    fn oversized_uploads_are_refused() {
        let (store, _dir) = store();
        let mut huge = PNG.to_vec();
        huge.resize(MAX_BYTES + 1, 0);
        assert!(matches!(
            store.store("huge", &huge).unwrap_err(),
            WallpaperError::TooLarge { .. }
        ));
    }

    #[test]
    fn ids_cannot_escape_the_directory() {
        let (store, _dir) = store();
        for id in ["../escape", "..", ".", "", "with/slash", "a\0b"] {
            assert!(
                matches!(store.store(id, PNG), Err(WallpaperError::InvalidId(_))),
                "{id:?} should be refused"
            );
        }
    }

    #[test]
    fn replacing_across_formats_leaves_one_file() {
        let (store, _dir) = store();
        store.store("wall", PNG).unwrap();
        store.store("wall", JPEG).unwrap();

        let listed = store.list();
        assert_eq!(listed.len(), 1, "the PNG should have been removed");
        assert_eq!(listed[0].content_type, "image/jpeg");
        assert_eq!(store.read("wall").unwrap().1, Format::Jpeg);
    }

    #[test]
    fn lists_in_a_stable_order() {
        let (store, _dir) = store();
        for id in ["zulu", "alpha", "mike"] {
            store.store(id, PNG).unwrap();
        }
        let ids: Vec<String> = store.list().into_iter().map(|w| w.id).collect();
        assert_eq!(ids, vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    fn removing_works_and_is_reported_once_gone() {
        let (store, _dir) = store();
        store.store("gone", PNG).unwrap();
        store.remove("gone").unwrap();
        assert!(matches!(
            store.read("gone").unwrap_err(),
            WallpaperError::NotFound(_)
        ));
    }

    #[test]
    fn listing_an_absent_directory_is_empty_not_an_error() {
        let (store, _dir) = store();
        assert!(store.list().is_empty());
    }
}
