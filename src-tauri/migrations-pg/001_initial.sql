-- Cadenza PostgreSQL schema — Fase C.
--
-- Mirrors the SQLite schema (`migrations/001_initial.sql`) row-for-row.
-- Differences:
--   • Postgres uses BIGINT for the millisecond epoch fields (same as
--     SQLite's INTEGER — width is identical, just a different name).
--   • CHECK constraints with the same allowed values.
--
-- We deliberately do NOT use TIMESTAMPTZ here so the wire format
-- (`created_at_ms: i64` in `cadenza-proto`) stays a single integer
-- across both backends and the file backend's `Proposta.created_at_ms`.

CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    titulo          TEXT NOT NULL,
    estado          TEXT NOT NULL
        CHECK (estado IN ('a_fazer','fazendo','aguardando_revisao','feito')),
    responsavel     TEXT NOT NULL DEFAULT 'humano',
    body            TEXT NOT NULL DEFAULT '',
    created_at_ms   BIGINT NOT NULL,
    updated_at_ms   BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tasks_estado ON tasks(estado);

CREATE TABLE IF NOT EXISTS propostas (
    proposta_id      TEXT PRIMARY KEY,
    idempotency_key  TEXT NOT NULL UNIQUE,
    parent           TEXT,
    title            TEXT NOT NULL,
    repro            TEXT NOT NULL,
    file             TEXT NOT NULL,
    what_failed      TEXT NOT NULL,
    action           TEXT NOT NULL,
    created_at_ms    BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_propostas_idemp ON propostas(idempotency_key);

CREATE TABLE IF NOT EXISTS decisoes (
    proposta_id    TEXT PRIMARY KEY,
    decisao        TEXT NOT NULL
        CHECK (decisao IN ('aceita','rejeitada','mesclada')),
    task_id        TEXT,
    autor          TEXT NOT NULL,
    decided_at_ms  BIGINT NOT NULL,
    FOREIGN KEY (proposta_id) REFERENCES propostas(proposta_id)
);
