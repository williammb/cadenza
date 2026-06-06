-- Cadenza PostgreSQL schema — review packages. Mirrors
-- migrations/004_reviews.sql with PG dialect (BIGINT ms, IF NOT EXISTS,
-- JSONB payload). One row per `done` attempt; identity = (task_id, attempt);
-- dedup key = (task_id, idempotency_key).

CREATE TABLE IF NOT EXISTS review_packages (
    task_id          TEXT NOT NULL,
    attempt          BIGINT NOT NULL,
    idempotency_key  TEXT NOT NULL,
    status           TEXT NOT NULL
        CHECK (status IN
            ('pending','superseded','aprovado','alteracoes_solicitadas')),
    payload          JSONB NOT NULL,
    created_at_ms    BIGINT NOT NULL,
    PRIMARY KEY (task_id, attempt),
    UNIQUE (task_id, idempotency_key),
    FOREIGN KEY (task_id) REFERENCES tasks(id)
);

CREATE INDEX IF NOT EXISTS idx_review_packages_task ON review_packages(task_id);
