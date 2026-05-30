//! Image attachments stored on the local filesystem.
//!
//! Attachments live under `~/.cadenza/attachments/<kind>/<owner_id>/<hash>.<ext>`
//! where `kind` is `tasks` or `ideias`. The bytes never enter the store
//! backend (file / SQLite / Postgres) — only the relative path
//! `attachments/<kind>/<owner_id>/<hash>.<ext>` is written into the
//! task/ideia body markdown, so behaviour is identical across backends.
//!
//! The filename is the content hash, which makes saves idempotent: pasting
//! the same image twice reuses the existing file.
//!
//! Only PNG, JPEG, GIF and WebP are accepted, detected by *magic bytes*
//! (the extension is never trusted), capped at 5 MB each. SVG is
//! deliberately excluded (script-injection risk).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Per-image hard cap. Mirrored in the UI for early feedback, but this is
/// the source of truth.
const MAX_BYTES: usize = 5 * 1024 * 1024;

/// Errors are surfaced to the UI as stable i18n keys (see `commands.rs`);
/// the `Display` text here is English and only used for logs.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("unsupported image format")]
    UnsupportedFormat,
    #[error("image exceeds the maximum size of 5 MB")]
    TooLarge,
    #[error("invalid attachment path")]
    BadPath,
    #[error("attachment not found")]
    NotFound,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, AttachmentError>;

/// A detected image format. The variant fixes both the on-disk extension
/// and the MIME used for the `data:` URL in the preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    fn ext(self) -> &'static str {
        match self {
            ImageKind::Png => "png",
            ImageKind::Jpeg => "jpg",
            ImageKind::Gif => "gif",
            ImageKind::Webp => "webp",
        }
    }

    fn mime(self) -> &'static str {
        match self {
            ImageKind::Png => "image/png",
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Gif => "image/gif",
            ImageKind::Webp => "image/webp",
        }
    }

    /// Map a stored extension back to a MIME for the read path. Kept in
    /// sync with [`ImageKind::ext`].
    fn from_ext(ext: &str) -> Option<Self> {
        match ext {
            "png" => Some(ImageKind::Png),
            "jpg" | "jpeg" => Some(ImageKind::Jpeg),
            "gif" => Some(ImageKind::Gif),
            "webp" => Some(ImageKind::Webp),
            _ => None,
        }
    }
}

/// Identify an image purely from its leading bytes. Returns `None` for
/// anything not in the allowlist (including SVG, which has no binary
/// signature).
fn detect(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        Some(ImageKind::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageKind::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(ImageKind::Gif)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageKind::Webp)
    } else {
        None
    }
}

/// Validate a `kind` (must be `tasks` or `ideias`) and an `owner_id`
/// (single safe path component — no separators, `..`, NUL, or empty).
fn check_owner(kind: &str, owner_id: &str) -> Result<()> {
    if kind != "tasks" && kind != "ideias" {
        return Err(AttachmentError::BadPath);
    }
    if owner_id.is_empty()
        || owner_id.len() > 128
        || owner_id
            .chars()
            .any(|c| c == '/' || c == '\\' || c == ':' || c == '\0' || c == '.')
    {
        // Note: '.' is rejected wholesale here — task/ideia ids never
        // contain one (they are `T-19`, `I-<uuid-simple>`), so this also
        // rules out `.` and `..` without a special case.
        return Err(AttachmentError::BadPath);
    }
    Ok(())
}

/// Root of all attachments: `~/.cadenza/attachments`.
fn root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza")
        .join("attachments")
}

/// Persist `bytes` for `(kind, owner_id)` and return the relative path
/// `attachments/<kind>/<owner_id>/<hash>.<ext>` to embed in the body.
pub fn save(kind: &str, owner_id: &str, bytes: &[u8]) -> Result<String> {
    save_in(&root(), kind, owner_id, bytes)
}

/// Read an attachment by its body-relative path, returning `(mime, bytes)`.
pub fn read(rel_path: &str) -> Result<(String, Vec<u8>)> {
    read_in(&root(), rel_path)
}

/// Best-effort removal of an owner's whole attachment directory. Logs on
/// failure in English but never propagates — the owning task/ideia is
/// already being deleted and a stray dir only costs disk bytes.
pub fn delete_owner(kind: &str, owner_id: &str) {
    if check_owner(kind, owner_id).is_err() {
        return;
    }
    let dir = root().join(kind).join(owner_id);
    if !dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        tracing::warn!(error = ?e, dir = %dir.display(), "failed to delete attachment dir");
    }
}

fn save_in(root: &Path, kind: &str, owner_id: &str, bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_BYTES {
        return Err(AttachmentError::TooLarge);
    }
    let format = detect(bytes).ok_or(AttachmentError::UnsupportedFormat)?;
    check_owner(kind, owner_id)?;

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    // 16 bytes (32 hex chars) is plenty to avoid content collisions while
    // keeping the filename short.
    let hash: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();

    let dir = root.join(kind).join(owner_id);
    std::fs::create_dir_all(&dir)?;
    let filename = format!("{hash}.{}", format.ext());
    let path = dir.join(&filename);
    // Content-addressed: identical bytes hash to the same name, so an
    // existing file is already the right content — skip the rewrite.
    if !path.exists() {
        std::fs::write(&path, bytes)?;
    }
    Ok(format!("attachments/{kind}/{owner_id}/{filename}"))
}

fn read_in(root: &Path, rel_path: &str) -> Result<(String, Vec<u8>)> {
    let path = resolve_under_root(root, rel_path)?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AttachmentError::NotFound)
        }
        Err(e) => return Err(AttachmentError::Io(e)),
    };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = ImageKind::from_ext(&ext)
        .map(|k| k.mime())
        .unwrap_or("application/octet-stream");
    Ok((mime.to_string(), bytes))
}

/// Resolve a body-relative `attachments/<kind>/<id>/<file>` path safely
/// under `root`, rejecting traversal (`..`), absolute paths, and
/// separators that could escape the attachments tree.
fn resolve_under_root(root: &Path, rel_path: &str) -> Result<PathBuf> {
    let mut comps = rel_path.split('/');
    // The body always stores the leading `attachments/` segment; the
    // `root` already points at that dir, so consume and verify it.
    if comps.next() != Some("attachments") {
        return Err(AttachmentError::BadPath);
    }
    let mut path = root.to_path_buf();
    let mut any = false;
    for comp in comps {
        if comp.is_empty()
            || comp == "."
            || comp == ".."
            || comp.contains('\\')
            || comp.contains(':')
            || comp.contains('\0')
        {
            return Err(AttachmentError::BadPath);
        }
        path.push(comp);
        any = true;
    }
    if !any {
        return Err(AttachmentError::BadPath);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal valid headers for each accepted format.
    fn png() -> Vec<u8> {
        let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(b"rest-of-png");
        v
    }
    fn jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend_from_slice(b"jfif");
        v
    }
    fn gif() -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(b"....");
        v
    }
    fn webp() -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(b"VP8 ");
        v
    }

    #[test]
    fn detects_each_format() {
        assert_eq!(detect(&png()), Some(ImageKind::Png));
        assert_eq!(detect(&jpeg()), Some(ImageKind::Jpeg));
        assert_eq!(detect(&gif()), Some(ImageKind::Gif));
        assert_eq!(detect(&webp()), Some(ImageKind::Webp));
    }

    #[test]
    fn rejects_unsupported_format() {
        // SVG / arbitrary text has no accepted signature.
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
        assert!(detect(&svg).is_none());
        let root = tempfile::tempdir().unwrap();
        let err = save_in(root.path(), "tasks", "T-1", &svg).unwrap_err();
        assert!(matches!(err, AttachmentError::UnsupportedFormat));
    }

    #[test]
    fn rejects_oversized() {
        let root = tempfile::tempdir().unwrap();
        let mut big = png();
        big.resize(MAX_BYTES + 1, 0);
        let err = save_in(root.path(), "tasks", "T-1", &big).unwrap_err();
        assert!(matches!(err, AttachmentError::TooLarge));
    }

    #[test]
    fn save_then_read_roundtrips() {
        let root = tempfile::tempdir().unwrap();
        let bytes = png();
        let rel = save_in(root.path(), "tasks", "T-7", &bytes).unwrap();
        assert!(rel.starts_with("attachments/tasks/T-7/"));
        assert!(rel.ends_with(".png"));
        let (mime, got) = read_in(root.path(), &rel).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(got, bytes);
    }

    #[test]
    fn hash_is_stable_and_dedups() {
        let root = tempfile::tempdir().unwrap();
        let bytes = jpeg();
        let a = save_in(root.path(), "tasks", "T-2", &bytes).unwrap();
        let b = save_in(root.path(), "tasks", "T-2", &bytes).unwrap();
        assert_eq!(a, b, "identical bytes must map to the same path");
        // Only one file on disk for the owner.
        let dir = root.path().join("tasks").join("T-2");
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn rejects_bad_kind_and_owner() {
        let root = tempfile::tempdir().unwrap();
        let bytes = png();
        assert!(matches!(
            save_in(root.path(), "evil", "T-1", &bytes),
            Err(AttachmentError::BadPath)
        ));
        assert!(matches!(
            save_in(root.path(), "tasks", "../escape", &bytes),
            Err(AttachmentError::BadPath)
        ));
        assert!(matches!(
            save_in(root.path(), "tasks", "a/b", &bytes),
            Err(AttachmentError::BadPath)
        ));
    }

    #[test]
    fn read_rejects_path_traversal() {
        let root = tempfile::tempdir().unwrap();
        for evil in [
            "attachments/../../secret",
            "attachments/tasks/../../../etc/passwd",
            "../outside",
            "attachments/tasks/T-1/..",
            "/abs/path",
        ] {
            assert!(
                matches!(read_in(root.path(), evil), Err(AttachmentError::BadPath)),
                "should reject {evil}"
            );
        }
    }

    #[test]
    fn read_missing_is_not_found() {
        let root = tempfile::tempdir().unwrap();
        let err = read_in(root.path(), "attachments/tasks/T-1/deadbeef.png").unwrap_err();
        assert!(matches!(err, AttachmentError::NotFound));
    }

    #[test]
    fn delete_owner_removes_dir() {
        let root = tempfile::tempdir().unwrap();
        let rel = save_in(root.path(), "ideias", "I-1", &gif()).unwrap();
        let dir = root.path().join("ideias").join("I-1");
        assert!(dir.exists());
        // Use the inner removal directly so the test stays inside the
        // tempdir root rather than the real home dir.
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!dir.exists());
        assert!(matches!(
            read_in(root.path(), &rel),
            Err(AttachmentError::NotFound)
        ));
    }
}
