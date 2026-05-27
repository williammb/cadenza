-- Cadenza SQLite schema — adição da entidade `Ideia` (Inbox).
--
-- Ideias são entidades separadas de tasks: não compartilham id/estado e
-- não passam pelo formato Node.js legacy. Schema novo, livre.

CREATE TABLE ideias (
    id              TEXT PRIMARY KEY,
    titulo          TEXT NOT NULL,
    body            TEXT NOT NULL DEFAULT '',
    project_id      TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pendente'
        CHECK (status IN ('pendente','destrinchada','arquivada')),
    created_at_ms   INTEGER NOT NULL
);

CREATE INDEX idx_ideias_project ON ideias(project_id);
CREATE INDEX idx_ideias_status  ON ideias(status);
