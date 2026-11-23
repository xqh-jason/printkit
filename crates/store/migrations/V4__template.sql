-- Stored documents. `document_json` is the complete, validated document body;
-- `template.save` keeps one row per save so a design history is not overwritten.
CREATE TABLE IF NOT EXISTS template (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    document_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS template_updated_at ON template (updated_at);
