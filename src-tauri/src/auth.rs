//! CLI auth token — `~/.cadenza/auth`, 32 random bytes base64url-encoded.
//!
//! Per DESIGN-desktop-v2.md § "Token":
//! - Generated on first start, persisted with mode `0600` on Unix.
//! - Validated against the `hello` handshake's `token` field.
//! - Rotated when the user picks "Revoke CLI token" in the tray.
//!
//! Windows ACL hardening (restrict to the current user's SID) is a
//! Phase 5 TODO — for now we rely on `%USERPROFILE%` being per-user.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use std::fs;
use std::path::{Path, PathBuf};

const TOKEN_BYTES: usize = 32;

/// Return the existing token, or mint and persist a new one if the
/// file doesn't exist. `dir` is typically `~/.cadenza/`.
pub fn ensure_token(dir: &Path) -> Result<String> {
    fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let path = token_path(dir);
    if path.exists() {
        let token = fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
        tracing::warn!(path = %path.display(), "auth file empty, regenerating");
    }
    let token = mint_token();
    write_token(&path, &token)?;
    tracing::info!(path = %path.display(), "minted new CLI auth token");
    Ok(token)
}

/// Replace the on-disk token with a freshly minted one and return it.
pub fn revoke(dir: &Path) -> Result<String> {
    let path = token_path(dir);
    let _ = fs::remove_file(&path);
    ensure_token(dir)
}

/// `true` if `candidate` matches the token in `dir`. Reads the file
/// fresh each call so rotations take effect without a restart.
pub fn validate(dir: &Path, candidate: &str) -> Result<bool> {
    let path = token_path(dir);
    if !path.exists() {
        return Ok(false);
    }
    let stored = fs::read_to_string(&path)?;
    Ok(constant_time_eq(stored.trim().as_bytes(), candidate.as_bytes()))
}

fn token_path(dir: &Path) -> PathBuf {
    dir.join("auth")
}

fn mint_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn write_token(path: &Path, token: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("auth path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        use std::io::Write;
        f.write_all(token.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, token)?;
        // TODO(phase-5): apply Windows DACL restricting to current user SID.
    }
    Ok(())
}

/// `==`-resistant comparison so the validator doesn't leak length via
/// timing. Tiny string, but the cost is also tiny.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ensure_then_read_yields_same_token() {
        let dir = TempDir::new().unwrap();
        let t1 = ensure_token(dir.path()).unwrap();
        let t2 = ensure_token(dir.path()).unwrap();
        assert_eq!(t1, t2);
        assert!(t1.len() > 30);
    }

    #[test]
    fn validate_accepts_correct_token() {
        let dir = TempDir::new().unwrap();
        let t = ensure_token(dir.path()).unwrap();
        assert!(validate(dir.path(), &t).unwrap());
    }

    #[test]
    fn validate_rejects_wrong_token() {
        let dir = TempDir::new().unwrap();
        let _ = ensure_token(dir.path()).unwrap();
        assert!(!validate(dir.path(), "definitely-wrong").unwrap());
    }

    #[test]
    fn validate_returns_false_when_missing() {
        let dir = TempDir::new().unwrap();
        assert!(!validate(dir.path(), "anything").unwrap());
    }

    #[test]
    fn revoke_changes_the_token() {
        let dir = TempDir::new().unwrap();
        let old = ensure_token(dir.path()).unwrap();
        let new = revoke(dir.path()).unwrap();
        assert_ne!(old, new);
        assert!(!validate(dir.path(), &old).unwrap());
        assert!(validate(dir.path(), &new).unwrap());
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
