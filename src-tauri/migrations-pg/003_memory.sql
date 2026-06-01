-- Cadenza PostgreSQL schema — memória compartilhada por projeto (T-34).
-- Espelha `migrations/003_memory.sql`.

CREATE TABLE IF NOT EXISTS memory_items (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL,
    texto         TEXT NOT NULL,
    origem_task   TEXT,
    criado_em     BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_items_project ON memory_items(project_id);

CREATE TABLE IF NOT EXISTS memory_suggestions (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL,
    criado_em     BIGINT NOT NULL,
    kind_json     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_suggestions_project ON memory_suggestions(project_id);
