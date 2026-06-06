-- Additive: optional Jira identity on tasks + jira_issues cache table.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS jira_site TEXT;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS jira_issue_id TEXT;

CREATE TABLE IF NOT EXISTS jira_issues (
    jira_site        TEXT NOT NULL,
    jira_issue_id    TEXT NOT NULL,
    jira_key         TEXT NOT NULL,
    project_id       TEXT,
    analysis_run_id  TEXT,
    secret_hash      TEXT,
    secret_expiry_ms BIGINT,
    secret_status    TEXT,
    raw_adf          TEXT,
    branch_name      TEXT,
    worktree_path    TEXT,
    base_sha         TEXT,
    worktree_state   TEXT,
    created_at_ms    BIGINT NOT NULL,
    updated_at_ms    BIGINT NOT NULL,
    PRIMARY KEY (jira_site, jira_issue_id)
);

CREATE INDEX IF NOT EXISTS idx_tasks_jira ON tasks(jira_site, jira_issue_id);
