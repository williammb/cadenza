-- Cadenza SQLite schema — Fase B.
--
-- Mirrors the file-backed format (frozen for Node.js task-ai compat):
--   • estado values stay PT canonical
--   • field names stay PT (titulo, responsavel)
--   • idempotency_key drives `propose` dedup, same as the JSON files
--
-- Tables are tied to `~/.cadenza/cadenza.db` (single-file DB by design,
-- per the user's MVP choice). The Node.js side cannot read this backend
-- — that tradeoff is documented in DESIGN-desktop-v4.md.

CREATE TABLE tasks (
    id              TEXT PRIMARY KEY,
    titulo          TEXT NOT NULL,
    estado          TEXT NOT NULL
        CHECK (estado IN ('a_fazer','fazendo','aguardando_revisao','feito')),
    responsavel     TEXT NOT NULL DEFAULT 'humano',
    body            TEXT NOT NULL DEFAULT '',
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
);

CREATE INDEX idx_tasks_estado ON tasks(estado);

CREATE TABLE propostas (
    proposta_id      TEXT PRIMARY KEY,
    idempotency_key  TEXT NOT NULL UNIQUE,
    parent           TEXT,
    title            TEXT NOT NULL,
    repro            TEXT NOT NULL,
    file             TEXT NOT NULL,
    what_failed      TEXT NOT NULL,
    action           TEXT NOT NULL,
    created_at_ms    INTEGER NOT NULL
);

CREATE INDEX idx_propostas_idemp ON propostas(idempotency_key);

CREATE TABLE decisoes (
    proposta_id    TEXT PRIMARY KEY,
    decisao        TEXT NOT NULL
        CHECK (decisao IN ('aceita','rejeitada','mesclada')),
    task_id        TEXT,
    autor          TEXT NOT NULL,
    decided_at_ms  INTEGER NOT NULL,
    FOREIGN KEY (proposta_id) REFERENCES propostas(proposta_id)
);
