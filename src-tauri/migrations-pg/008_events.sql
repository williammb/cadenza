-- Cadenza PostgreSQL schema — run timeline / audit event log (feature #8).
-- Mirrors migrations/008_events.sql with PG dialect (BIGINT ms, IF NOT
-- EXISTS, JSONB payload, IDENTITY rowid). Append-only; no FK to tasks(id)
-- so an audit record survives task deletion.

CREATE TABLE IF NOT EXISTS events (
    seq           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id            TEXT NOT NULL UNIQUE,
    task_id       TEXT,
    kind          TEXT NOT NULL,
    payload       JSONB NOT NULL,
    created_at_ms BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at_ms);
CREATE INDEX IF NOT EXISTS idx_events_task ON events(task_id);
