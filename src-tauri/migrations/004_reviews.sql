-- Cadenza SQLite schema — review packages (PLAN §C.9, §F.17, §F.18).
--
-- One row per `done` attempt. Identity = (task_id, attempt); the durable
-- dedup key is (task_id, idempotency_key) -> idempotent upsert. The whole
-- ReviewPackage is stored as JSON text in `payload` (mirroring how the file
-- backend serializes the struct and how memory_suggestions.kind_json stores
-- an evolving enum), so the rich snapshot schema can evolve without a
-- migration per field. The queryable lifecycle fields (status, created_at_ms)
-- are promoted to real columns for cheap latest/supersede queries.

CREATE TABLE review_packages (
    task_id          TEXT NOT NULL,
    attempt          INTEGER NOT NULL,
    idempotency_key  TEXT NOT NULL,
    status           TEXT NOT NULL
        CHECK (status IN
            ('pending','superseded','aprovado','alteracoes_solicitadas')),
    payload          TEXT NOT NULL,
    created_at_ms    INTEGER NOT NULL,
    PRIMARY KEY (task_id, attempt),
    UNIQUE (task_id, idempotency_key),
    FOREIGN KEY (task_id) REFERENCES tasks(id)
);

CREATE INDEX idx_review_packages_task ON review_packages(task_id);
