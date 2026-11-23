-- Usage statistics, one row per print attempt. Only metadata is stored: the
-- document body never reaches this table, which is why `trace_id` plus `seq`
-- is enough to join a row back to the JSONL stream for diagnosis.
CREATE TABLE IF NOT EXISTS print_record (
    id INTEGER PRIMARY KEY,
    trace_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    at TEXT NOT NULL,
    origin TEXT NOT NULL,
    template TEXT,
    tags TEXT NOT NULL DEFAULT '[]',
    printer TEXT,
    pages INTEGER NOT NULL,
    copies INTEGER NOT NULL,
    status TEXT NOT NULL,
    dur_ms INTEGER NOT NULL,
    error_code TEXT
);

CREATE INDEX IF NOT EXISTS print_record_at ON print_record (at);
CREATE INDEX IF NOT EXISTS print_record_origin ON print_record (origin);
