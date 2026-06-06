CREATE TABLE IF NOT EXISTS jira_review_packages (
    jira_site        TEXT NOT NULL,
    jira_issue_id    TEXT NOT NULL,
    attempt          BIGINT NOT NULL,
    idempotency_key  TEXT NOT NULL,
    status           TEXT NOT NULL
        CHECK (status IN
            ('pending','superseded','aprovado','alteracoes_solicitadas')),
    payload          JSONB NOT NULL,
    created_at_ms    BIGINT NOT NULL,
    -- No FK to jira_issues: aggregate review packages are RETAINED as an
    -- audit trail after `jira_discard` deletes the issue record, so the
    -- parent row can legitimately disappear while the package remains.
    PRIMARY KEY (jira_site, jira_issue_id, attempt),
    UNIQUE (jira_site, jira_issue_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS idx_jira_review_packages_issue
    ON jira_review_packages(jira_site, jira_issue_id);
