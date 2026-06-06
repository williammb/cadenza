-- Additive: optional Jira identity on propostas. A server-stamped proposal
-- (jira_materialize) must keep its (jira_site, jira_issue_id) through the
-- human accept on the SQL backends, so create_task_from_proposta can copy it
-- onto the Task and the lazy worktree hook fires. The file backend already
-- round-trips these via JSON; this closes the gap for SQLite/Postgres.
ALTER TABLE propostas ADD COLUMN IF NOT EXISTS jira_site TEXT;
ALTER TABLE propostas ADD COLUMN IF NOT EXISTS jira_issue_id TEXT;
