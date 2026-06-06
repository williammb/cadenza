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
    /// GitHub Copilot CLI (`copilot`) — terminal TUI coding agent.
    Copilot,
    /// Antigravity CLI (`agy`) — Google's Gemini-based terminal TUI
    /// coding agent. Runs interactively under a PTY like the others.
    Antigravity,
    /// OpenCode CLI (`opencode`) — terminal TUI coding agent with
    /// provider/model ids and resumable sessions.
    #[serde(rename = "opencode")]
    OpenCode,
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

/// Jira Cloud connection settings stored in `config.json`. The API token
/// is intentionally absent — it lives in the OS keyring, looked up via
/// `secrets::jira_account_for(base_url)`. `base_url` is validated by
/// `crate::jira::config::validate_base_url` on config load and again when
/// the HTTP client is built (single source of truth for the host rule).
// `Default` is derived (both fields are empty strings); clippy rejects the
// hand-written impl the contract sketched as `derivable_impls`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JiraConfig {
    /// e.g. "https://your-org.atlassian.net" — token lives in the OS keyring, never here.
    pub base_url: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agente {
    pub kind: AgenteKind,
    /// Override the CLI path. If `None`, look up by name on `PATH`.
    #[serde(default)]
    pub command: Option<PathBuf>,
}

/// Per-project, app-side quality contract (PLAN §A). GUI-editable in
/// `ui/settings.js`; never committed to the repo (explicit user choice).
/// Absent/empty profile ⇒ a `no_validation` review package.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QualityProfile {
    /// Display order is the vector order; it does NOT affect `contract_version`
    /// (the hash is order-independent — see `contract_version`). Duplicate or
    /// empty `id`s are rejected by `Config::validate`.
    #[serde(default)]
    pub checks: Vec<QualityCheck>,
}

/// One validation command the agent is expected to run before `done`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QualityCheck {
    /// Stable, unique-within-profile identifier (e.g. "clippy"). Used to
    /// correlate reported evidence back to the contract and as the sort key
    /// for the semantic `contract_version` hash. Validated: non-empty,
    /// trimmed-unique.
    pub id: String,
    /// Human label for the UI (e.g. "Clippy (deny warnings)").
    pub name: String,
    /// The command the agent runs. The app NEVER executes this (PLAN key
    /// decisions); it is contract + display only.
    pub cmd: String,
    /// Always-required when true.
    #[serde(default)]
    pub required: bool,
    /// Repo-relative POSIX globset patterns; the check becomes required if
    /// `required` OR any pattern matches the changed-file set (PLAN §A.1).
    #[serde(default)]
    pub required_if_changed: Vec<String>,
}

impl QualityProfile {
    /// Semantic, order-independent contract hash (PLAN §A.2). Hashes the
    /// check list **sorted by `id`** over only the fields that affect
    /// required-ness — `id`, `cmd`, `required`, `required_if_changed`
    /// (with each check's pattern list also sorted). Display order, `name`,
    /// and an empty profile all hash deterministically. Returns
    /// `"sha256:<hex>"`.
    pub fn contract_version(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut checks: Vec<&QualityCheck> = self.checks.iter().collect();
        checks.sort_by(|a, b| a.id.trim().cmp(b.id.trim()));
        let mut h = Sha256::new();
        for c in &checks {
            h.update(c.id.trim().as_bytes());
            h.update([0u8]);
            h.update(c.cmd.as_bytes());
            h.update([0u8]);
            h.update([u8::from(c.required)]);
            h.update([0u8]);
            let mut pats: Vec<&str> = c.required_if_changed.iter().map(|s| s.as_str()).collect();
            pats.sort_unstable();
            for p in pats {
                h.update(p.as_bytes());
                h.update([0u8]);
            }
            h.update([0x1eu8]); // record separator between checks
        }
        let digest = h.finalize();
        let mut s = String::from("sha256:");
        for b in digest {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    /// Per-project override of the global `agente`.
    #[serde(default)]
    pub agente: Option<Agente>,
    /// Default git branch this project's tasks branch off from. Pre-fills
    /// the "origin branch" in the task modal; falls back to the repo's
    /// current branch when unset. `None`/empty means "use current".
    #[serde(default)]
    pub default_branch: Option<String>,
    /// Color key for the board (e.g. "slate", "rust"). Resolved to a
    /// hex value by `ui/project-colors.js`. Shown only in the
    /// all-projects view as a left accent bar on cards.
    #[serde(default)]
    pub color: Option<String>,
    /// Per-project quality contract (PLAN §A.1). Absent/empty ⇒ no_validation.
    #[serde(default)]
    pub quality: Option<QualityProfile>,
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

    /// Jira Cloud connection (API token lives in the OS keyring, never here).
    /// `None` until the user configures Jira.
    #[serde(default)]
    pub jira: Option<JiraConfig>,

    /// Per-agent discovered model lists, cached so the ~15 s `/model`
    /// probe only runs when the user explicitly clicks "Carregar modelos"
    /// in Settings. Seeds the in-memory `AppState.agent_models` cache at
    /// startup. `None`/absent until the first discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_models: Option<Vec<crate::models::CachedModels>>,
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
            jira: None,
            agent_models: None,
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
            if let Some(q) = &p.quality {
                let mut seen = std::collections::HashSet::new();
                for (j, c) in q.checks.iter().enumerate() {
                    let id = c.id.trim();
                    if id.is_empty() {
                        return Err(anyhow!(
                            "project '{}' quality.checks[{}] has empty id",
                            p.id,
                            j
                        ));
                    }
                    if !seen.insert(id.to_string()) {
                        return Err(anyhow!(
                            "project '{}' has duplicate quality check id '{}'",
                            p.id,
                            id
                        ));
                    }
                }
            }
        }
        if let Some(j) = &self.jira {
            crate::jira::config::validate_base_url(&j.base_url)
                .map_err(|e| anyhow!("jira.base_url invalid: {e}"))?;
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
    fn config_with_invalid_jira_base_url_fails_validate() {
        let f = write_tmp(
            r#"{"data_version":1,"jira":{"base_url":"http://evil.example.com","email":"a@b.c"}}"#,
        );
        let err = Config::load_from(f.path()).unwrap_err();
        assert!(
            err.to_string().contains("jira.base_url invalid"),
            "got: {err}"
        );
    }

    #[test]
    fn config_jira_section_roundtrips_json() {
        let f = write_tmp(
            r#"{"data_version":1,"jira":{"base_url":"https://acme.atlassian.net","email":"dev@acme.io"}}"#,
        );
        let cfg = Config::load_from(f.path()).unwrap();
        let j = cfg.jira.as_ref().unwrap();
        assert_eq!(j.base_url, "https://acme.atlassian.net");
        assert_eq!(j.email, "dev@acme.io");
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("acme.atlassian.net"), "got: {json}");
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
    fn antigravity_kind_roundtrips() {
        let f = write_tmp(r#"{"data_version":1,"agente":{"kind":"antigravity"}}"#);
        let cfg = Config::load_from(f.path()).unwrap();
        assert_eq!(cfg.agente.as_ref().unwrap().kind, AgenteKind::Antigravity);
        // And serializes back to the snake_case wire form.
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"antigravity\""), "got: {json}");
    }

    #[test]
    fn opencode_kind_roundtrips() {
        let f = write_tmp(r#"{"data_version":1,"agente":{"kind":"opencode"}}"#);
        let cfg = Config::load_from(f.path()).unwrap();
        assert_eq!(cfg.agente.as_ref().unwrap().kind, AgenteKind::OpenCode);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"opencode\""), "got: {json}");
    }

    #[test]
    fn copilot_kind_roundtrips() {
        let f = write_tmp(r#"{"data_version":1,"agente":{"kind":"copilot"}}"#);
        let cfg = Config::load_from(f.path()).unwrap();
        assert_eq!(cfg.agente.as_ref().unwrap().kind, AgenteKind::Copilot);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"copilot\""), "got: {json}");
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

    fn check(id: &str, cmd: &str, required: bool, pats: &[&str]) -> QualityCheck {
        QualityCheck {
            id: id.into(),
            name: format!("{id} name"),
            cmd: cmd.into(),
            required,
            required_if_changed: pats.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn contract_version_is_order_independent() {
        let a = QualityProfile {
            checks: vec![
                check("clippy", "cargo clippy", true, &["src/**"]),
                check("fmt", "cargo fmt", false, &[]),
            ],
        };
        let b = QualityProfile {
            checks: vec![
                check("fmt", "cargo fmt", false, &[]),
                check("clippy", "cargo clippy", true, &["src/**"]),
            ],
        };
        assert_eq!(a.contract_version(), b.contract_version());
    }

    #[test]
    fn contract_version_ignores_name_and_pattern_order() {
        let a = QualityProfile {
            checks: vec![check("clippy", "cargo clippy", true, &["a/**", "b/**"])],
        };
        let mut b = a.clone();
        b.checks[0].name = "Totally different label".into();
        b.checks[0].required_if_changed = vec!["b/**".into(), "a/**".into()];
        assert_eq!(a.contract_version(), b.contract_version());
    }

    #[test]
    fn contract_version_is_sensitive_to_fields() {
        let base = QualityProfile {
            checks: vec![check("clippy", "cargo clippy", true, &["src/**"])],
        };
        let mut cmd = base.clone();
        cmd.checks[0].cmd = "cargo clippy --fix".into();
        let mut req = base.clone();
        req.checks[0].required = false;
        let mut pat = base.clone();
        pat.checks[0].required_if_changed = vec!["other/**".into()];
        let v = base.contract_version();
        assert_ne!(v, cmd.contract_version());
        assert_ne!(v, req.contract_version());
        assert_ne!(v, pat.contract_version());
    }

    #[test]
    fn empty_profile_hashes_deterministically() {
        let a = QualityProfile::default();
        let b = QualityProfile::default();
        assert_eq!(a.contract_version(), b.contract_version());
        assert!(a.contract_version().starts_with("sha256:"));
    }

    #[test]
    fn rejects_duplicate_quality_check_id() {
        let f = write_tmp(
            r#"{"data_version":1,"projects":[{"id":"p1","name":"P","path":".","quality":{"checks":[{"id":"x","name":"X","cmd":"a"},{"id":"x","name":"Y","cmd":"b"}]}}]}"#,
        );
        let err = Config::load_from(f.path()).unwrap_err();
        assert!(format!("{:#}", err).contains("duplicate quality check id"));
    }

    #[test]
    fn rejects_empty_quality_check_id() {
        let f = write_tmp(
            r#"{"data_version":1,"projects":[{"id":"p1","name":"P","path":".","quality":{"checks":[{"id":"  ","name":"X","cmd":"a"}]}}]}"#,
        );
        let err = Config::load_from(f.path()).unwrap_err();
        assert!(format!("{:#}", err).contains("empty id"));
    }

    #[test]
    fn quality_profile_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        let cfg = Config {
            projects: vec![Project {
                id: "p1".into(),
                name: "P".into(),
                path: ".".into(),
                agente: None,
                default_branch: None,
                color: None,
                quality: Some(QualityProfile {
                    checks: vec![check("clippy", "cargo clippy", true, &["src/**"])],
                }),
            }],
            ..Config::default()
        };
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        let q = loaded.projects[0].quality.as_ref().unwrap();
        assert_eq!(q.checks.len(), 1);
        assert_eq!(q.checks[0].id, "clippy");
        assert!(q.checks[0].required);
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
