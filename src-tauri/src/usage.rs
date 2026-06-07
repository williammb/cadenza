//! Cost/token telemetry adapters (feature #1).
//!
//! MEASURED usage only — never estimated. Each agent exposes usage
//! differently; this module starts with **Claude Code**, whose session
//! transcript is an append-only JSONL at
//! `~/.claude/projects/<encoded-cwd>/<conversation_id>.jsonl`. The file name
//! IS the conversation/session UUID, so we locate it by globbing across
//! project dirs rather than replicating Claude's cwd path-encoding.
//!
//! Every other agent (Codex/Copilot/agy/OpenCode) currently degrades to
//! "unavailable" (returns `None`) — their formats can be added as adapters
//! later, each backed by a fixture like the Claude one here.

use cadenza_proto::UsageObservation;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// `source` tag for the Claude adapter (recorded on the observation).
pub const CLAUDE_SOURCE: &str = "claude_session_jsonl";

fn claude_projects_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".claude")
        .join("projects")
}

/// Locate the Claude session JSONL named `<conversation_id>.jsonl` under any
/// `~/.claude/projects/*/` dir. Returns `None` when absent (graceful).
pub fn find_claude_session_file(conversation_id: &str) -> Option<PathBuf> {
    find_claude_session_file_in(&claude_projects_root(), conversation_id)
}

/// Testable core of [`find_claude_session_file`] with an explicit projects root.
fn find_claude_session_file_in(root: &Path, conversation_id: &str) -> Option<PathBuf> {
    let target = format!("{conversation_id}.jsonl");
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let candidate = entry.path().join(&target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Parse a Claude session JSONL into a measured [`UsageObservation`]. Returns
/// `None` when the file is unreadable or carries no usage.
///
/// Shape (verified against a real `~/.claude` transcript): each line is a JSON
/// object; assistant turns carry `message.usage` with `input_tokens`,
/// `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`,
/// and `message.model`.
///
/// Two correctness details that naive summing gets wrong:
/// - **Dedup by `message.id`.** Claude writes each content block of ONE
///   assistant turn (thinking / text / tool_use) as its own JSONL line, all
///   carrying the SAME `message.id` and the SAME (identical) `usage`. Counting
///   every line double/triple-counts the turn (~2x in practice). We count each
///   `message.id` once (lines without an id fall back to being counted).
/// - **`cache_read_input_tokens` is NOT additive.** It is the cumulative
///   cached context re-sent each turn (grows monotonically), so summing it
///   would count the same context N times. We take the LAST turn's value
///   (final cached-context size). `output`/`cache_creation`/`input` are
///   per-turn new work and ARE summed.
pub fn parse_claude_usage(path: &Path) -> Option<UsageObservation> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut obs = UsageObservation::new(CLAUDE_SOURCE.to_string());
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut last_cache_read: u64 = 0;
    let mut saw_usage = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(msg) = v.get("message") else {
            continue;
        };
        let Some(usage) = msg.get("usage") else {
            continue;
        };
        // Count each physical assistant turn once. A message split across
        // content-block lines repeats its id + usage; an absent id is rare and
        // falls through to being counted (legacy/odd lines still contribute).
        if let Some(id) = msg.get("id").and_then(|x| x.as_str()) {
            if !seen_ids.insert(id.to_string()) {
                continue;
            }
        }
        saw_usage = true;
        let u = |k: &str| {
            usage
                .get(k)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        obs.input_tokens = obs.input_tokens.saturating_add(u("input_tokens"));
        obs.output_tokens = obs.output_tokens.saturating_add(u("output_tokens"));
        obs.cache_creation_tokens = obs
            .cache_creation_tokens
            .saturating_add(u("cache_creation_input_tokens"));
        // Per-turn cumulative snapshot — keep the last, don't sum.
        last_cache_read = u("cache_read_input_tokens");
        if obs.model.is_none() {
            if let Some(m) = msg.get("model").and_then(|x| x.as_str()) {
                obs.model = Some(m.to_string());
            }
        }
    }
    obs.cache_read_tokens = last_cache_read;
    saw_usage.then_some(obs)
}

/// Measured usage for a Claude run by conversation id, or `None` when the
/// session file can't be found/parsed.
pub fn claude_usage(conversation_id: &str) -> Option<UsageObservation> {
    find_claude_session_file(conversation_id).and_then(|p| parse_claude_usage(&p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // A fixture matching the real Claude transcript shape (verified against a
    // live ~/.claude session): assistant lines with message.usage + model,
    // plus non-usage lines that must be skipped.
    const FIXTURE: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":100,"cache_creation_input_tokens":10,"cache_read_input_tokens":5,"output_tokens":40,"server_tool_use":{"web_search_requests":0}}}}"#,
        "\n",
        r#"{"type":"system","subtype":"info"}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":200,"output_tokens":60}}}"#,
        "\n",
    );

    #[test]
    fn parses_and_aggregates_claude_usage() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("conv.jsonl");
        fs::write(&f, FIXTURE).unwrap();
        let obs = parse_claude_usage(&f).expect("usage parsed");
        assert_eq!(obs.source, CLAUDE_SOURCE);
        assert_eq!(obs.model.as_deref(), Some("claude-opus-4-8"));
        // input/output/cache_creation are summed across turns.
        assert_eq!(obs.input_tokens, 107);
        assert_eq!(obs.output_tokens, 100);
        assert_eq!(obs.cache_creation_tokens, 10);
        // cache_read is the LAST turn's value (final context), NOT the sum.
        assert_eq!(obs.cache_read_tokens, 200);
        // total = input + output + cache_creation (excludes cache_read).
        assert_eq!(obs.total_tokens(), 217);
    }

    #[test]
    fn dedups_content_block_lines_sharing_a_message_id() {
        // One physical assistant turn split into 3 content-block lines, all
        // carrying the SAME message.id and the SAME usage — must be counted ONCE.
        let line = |ct: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"msg_1","model":"claude-opus-4-8","content":[{{"type":"{ct}"}}],"usage":{{"input_tokens":50,"cache_creation_input_tokens":4,"cache_read_input_tokens":300,"output_tokens":20}}}}}}"#
            )
        };
        let body = format!(
            "{}\n{}\n{}\n",
            line("thinking"),
            line("text"),
            line("tool_use")
        );
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("dup.jsonl");
        fs::write(&f, body).unwrap();
        let obs = parse_claude_usage(&f).expect("usage parsed");
        // Counted once, not 3x.
        assert_eq!(obs.input_tokens, 50);
        assert_eq!(obs.output_tokens, 20);
        assert_eq!(obs.cache_creation_tokens, 4);
        assert_eq!(obs.cache_read_tokens, 300);
    }

    #[test]
    fn no_usage_lines_yields_none() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("empty.jsonl");
        fs::write(&f, "{\"type\":\"user\"}\n\n{\"type\":\"system\"}\n").unwrap();
        assert!(parse_claude_usage(&f).is_none());
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("mixed.jsonl");
        fs::write(
            &f,
            concat!(
                "not json at all\n",
                r#"{"type":"assistant","message":{"usage":{"input_tokens":3,"output_tokens":4}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let obs = parse_claude_usage(&f).expect("usage parsed past the bad line");
        assert_eq!(obs.input_tokens, 3);
        assert_eq!(obs.output_tokens, 4);
    }

    #[test]
    fn finds_session_file_by_id_across_project_dirs() {
        let root = TempDir::new().unwrap();
        let proj = root.path().join("C--some-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("the-uuid.jsonl"), FIXTURE).unwrap();
        let found = find_claude_session_file_in(root.path(), "the-uuid").expect("found");
        assert!(found.ends_with("the-uuid.jsonl"));
        assert!(find_claude_session_file_in(root.path(), "missing").is_none());
    }
}
