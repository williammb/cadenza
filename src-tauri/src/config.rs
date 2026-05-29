//! `~/.cadenza/config.json` loader.
//!
//! Schema matches the existing Node.js system per DESIGN-desktop-v2.md
//! § "Compatibilidade com dados existentes" — same file, additive only.
//!
//! Wired into Tauri commands in Phase 2-3; allow dead_code until then.
#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `bytes` to `path` and `fsync` before returning. Used as the
/// first half of a tmp+rename atomic write so the data is durable on
/// disk before the rename publishes it (otherwise a power loss
/// between rename and the deferred data flush can leave a zero-byte
/// file post-reboot).
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Current on-disk schema version. Bumped only on a breaking layout
/// change; older versions auto-migrate, newer versions refuse to load.
pub const DATA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgenteKind {
    ClaudeCode,
    Codex,
}

/// Where Cadenza persists tasks + triage. The `files` backend keeps
/// the on-disk format frozen for Node.js `task-ai` compatibility;
/// `sqlite` and `postgres` are Cadenza-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageBackend {
    #[default]
    Files,
    Sqlite,
    Postgres,
}

/// User-facing SSL mode for Postgres connections. Matches sqlx's
/// `PgSslMode` 1:1 but lives in the config layer so config.json
/// doesn't pick up a sqlx-typed surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PgSslMode {
    Disable,
    Prefer,
    #[default]
    Require,
}

/// Postgres connection settings stored in `config.json`. The password
/// is intentionally absent — it lives in the OS keyring (Windows
/// Credential Manager / macOS Keychain / libsecret), looked up via
/// `secrets::account_for(user, host, port, database)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PgConfig {
    pub host: String,
    #[serde(default = "default_pg_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub ssl_mode: PgSslMode,
}

fn default_pg_port() -> u16 {
    5432
}

impl Default for PgConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_pg_port(),
            database: String::new(),
            user: String::new(),
            ssl_mode: PgSslMode::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agente {
    pub kind: AgenteKind,
    /// Override the CLI path. If `None`, look up by name on `PATH`.
    #[serde(default)]
    pub command: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    /// Per-project override of the global `agente`.
    #[serde(default)]
    pub agente: Option<Agente>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_data_version")]
    pub data_version: u32,

    /// Locale override for app UI and CLI output. Falls back through
    /// the resolution chain (see `cadenza_i18n::locale::resolve`).
    #[serde(default)]
    pub locale: Option<String>,

    /// Locale of the `skills/cadenza.<lang>.md` snippet written into the
    /// project. Defaults to the same as `locale` when `None`.
    #[serde(default)]
    pub skill_locale: Option<String>,

    #[serde(default)]
    pub projects: Vec<Project>,

    /// Global default agent. Per-project `agente` overrides this.
    #[serde(default)]
    pub agente: Option<Agente>,

    /// Project the board is currently filtered by. `None` means "all
    /// projects". The mapping task_id → project_id lives in
    /// `~/.cadenza/task-projects.json`, not here.
    #[serde(default)]
    pub active_project_id: Option<String>,

    /// Where tasks + triage are stored. Defaults to `files`. Changing
    /// this triggers a one-way migration during `AppState::init` so the
    /// new backend is fully populated before any read/write happens.
    #[serde(default)]
    pub storage_backend: StorageBackend,

    /// Postgres connection parameters (password lives in the OS
    /// keyring, never here). `None` when the user hasn't configured
    /// Postgres yet — `storage_backend = postgres` with a `None`
    /// `postgres` block falls back to files with a warning log.
    #[serde(default)]
    pub postgres: Option<PgConfig>,
}

fn default_data_version() -> u32 {
    DATA_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_version: DATA_VERSION,
            locale: None,
            skill_locale: None,
            projects: Vec::new(),
            agente: None,
            active_project_id: None,
            storage_backend: StorageBackend::default(),
            postgres: None,
        }
    }
}

impl Config {
    /// Load and validate the config at `path`.
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Config = serde_json::from_str(&text)
            .with_context(|| format!("parsing config at {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Atomic write to `path` — serializes as pretty JSON, writes to
    /// a sibling `.tmp` file, then renames into place. Same pattern as
    /// `triage::write_json_atomic`; kept private here so config writes
    /// don't depend on the triage module.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).context("serializing config")?;
        write_synced(&tmp, text.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Validate semantic constraints (data_version range, non-empty IDs).
    pub fn validate(&self) -> Result<()> {
        if self.data_version > DATA_VERSION {
            return Err(anyhow!(
                "config data_version is {} but this build only understands up to {}; install a newer Cadenza",
                self.data_version,
                DATA_VERSION
            ));
        }
        for (i, p) in self.projects.iter().enumerate() {
            if p.id.trim().is_empty() {
                return Err(anyhow!("projects[{}] has empty id", i));
            }
            if p.name.trim().is_empty() {
                return Err(anyhow!("project '{}' has empty name", p.id));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn write_tmp(json: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn loads_minimal_config() {
        let f = write_tmp(r#"{"data_version":1}"#);
        let cfg = Config::load_from(f.path()).unwrap();
        assert_eq!(cfg.data_version, 1);
        assert!(cfg.projects.is_empty());
        assert!(cfg.locale.is_none());
    }

    #[test]
    fn loads_full_config() {
        let json = r#"{
            "data_version": 1,
            "locale": "pt-BR",
            "skill_locale": "en",
            "projects": [
                {
                    "id": "task-ai",
                    "name": "Task AI",
                    "path": "C:/dev/task-ai",
                    "agente": { "kind": "claude_code" }
                }
            ],
            "agente": { "kind": "codex", "command": "C:/tools/codex.exe" }
        }"#;
        let f = write_tmp(json);
        let cfg = Config::load_from(f.path()).unwrap();
        assert_eq!(cfg.locale.as_deref(), Some("pt-BR"));
        assert_eq!(cfg.skill_locale.as_deref(), Some("en"));
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].id, "task-ai");
        assert_eq!(
            cfg.projects[0].agente.as_ref().unwrap().kind,
            AgenteKind::ClaudeCode
        );
        assert_eq!(cfg.agente.as_ref().unwrap().kind, AgenteKind::Codex);
    }

    #[test]
    fn rejects_future_data_version() {
        let f = write_tmp(r#"{"data_version":99}"#);
        let err = Config::load_from(f.path()).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("data_version"), "got: {msg}");
    }

    #[test]
    fn rejects_empty_project_id() {
        let f = write_tmp(r#"{"data_version":1,"projects":[{"id":"","name":"x","path":"."}]}"#);
        let err = Config::load_from(f.path()).unwrap_err();
        assert!(format!("{:#}", err).contains("empty id"));
    }

    #[test]
    fn rejects_invalid_json() {
        let f = write_tmp("not json");
        assert!(Config::load_from(f.path()).is_err());
    }

    #[test]
    fn missing_file_errors_with_path() {
        let err =
            Config::load_from(Path::new("C:/no-such-path-cadenza-test/config.json")).unwrap_err();
        assert!(format!("{:#}", err).contains("config.json"));
    }

    #[test]
    fn default_data_version_when_absent() {
        let f = write_tmp("{}");
        let cfg = Config::load_from(f.path()).unwrap();
        assert_eq!(cfg.data_version, DATA_VERSION);
    }

    #[test]
    fn rejects_empty_project_name() {
        let f = write_tmp(r#"{"data_version":1,"projects":[{"id":"p1","name":"","path":"."}]}"#);
        let err = Config::load_from(f.path()).unwrap_err();
        assert!(
            format!("{:#}", err).contains("empty name"),
            "got: {:#}",
            err
        );
    }

    #[test]
    fn save_to_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let cfg = Config {
            locale: Some("pt-BR".into()),
            skill_locale: Some("en".into()),
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.locale.as_deref(), Some("pt-BR"));
        assert_eq!(loaded.skill_locale.as_deref(), Some("en"));
        assert_eq!(loaded.data_version, DATA_VERSION);
    }
}
