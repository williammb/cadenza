-- Cadenza PostgreSQL schema — adição da entidade `Ideia` (Inbox).
-- Espelha `migrations/002_ideias.sql`.

CREATE TABLE IF NOT EXISTS ideias (
    id              TEXT PRIMARY KEY,
    titulo          TEXT NOT NULL,
    body            TEXT NOT NULL DEFAULT '',
    project_id      TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pendente'
        CHECK (status IN ('pendente','destrinchada','arquivada')),
    created_at_ms   BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_ideias_project ON ideias(project_id);
CREATE INDEX IF NOT EXISTS idx_ideias_status  ON ideias(status);
