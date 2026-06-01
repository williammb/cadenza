-- Cadenza SQLite schema — memória compartilhada por projeto (T-34).
--
-- Dois conceitos: itens curados da memória oficial (`memory_items`) e
-- sugestões pendentes aguardando curadoria (`memory_suggestions`). O
-- `kind` da sugestão (aprendizado vs op de reeval) é guardado como JSON
-- em `kind_json` para acompanhar o enum `SuggestionKind` sem uma coluna
-- por variante. Schema novo, livre.

CREATE TABLE memory_items (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL,
    texto         TEXT NOT NULL,
    origem_task   TEXT,
    criado_em     INTEGER NOT NULL
);

CREATE INDEX idx_memory_items_project ON memory_items(project_id);

CREATE TABLE memory_suggestions (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL,
    criado_em     INTEGER NOT NULL,
    kind_json     TEXT NOT NULL
);

CREATE INDEX idx_memory_suggestions_project ON memory_suggestions(project_id);
