-- Cadenza SQLite schema — run timeline / audit event log (feature #8).
--
-- Append-only: one row per RunEvent (agent started, PTY session ended,
-- done submitted, review decided, proposal decided). NEVER updated or
-- deleted. The whole RunEvent is stored as JSON text in `payload`
-- (mirroring review_packages / memory_suggestions) so the event schema can
-- evolve without a migration per field. Queryable fields (kind,
-- created_at_ms, task_id) are promoted to real columns. `seq` is a
-- monotonic rowid giving a stable insertion order for reads.
--
-- No FOREIGN KEY to tasks(id): an audit record must survive (and never be
-- blocked by) task deletion; task_id is a soft scope, nullable.

CREATE TABLE events (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT NOT NULL UNIQUE,
    task_id       TEXT,
    kind          TEXT NOT NULL,
    payload       TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_events_created ON events(created_at_ms);
CREATE INDEX idx_events_task ON events(task_id);
