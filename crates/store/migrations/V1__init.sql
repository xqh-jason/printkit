-- Run-time settings that the control plane may change; the process reads them
-- at startup. Keys are the configuration names without the table prefix,
-- for example `log_level`, plus `origins` holding a JSON array.
CREATE TABLE IF NOT EXISTS setting (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

-- Service authorisations. Only the SHA-256 of a token is stored; the plaintext
-- exists in the browser response once and in `admin-token` for the administrator.
CREATE TABLE IF NOT EXISTS auth (
    id INTEGER PRIMARY KEY,
    origin TEXT NOT NULL,
    token_sha256 TEXT NOT NULL UNIQUE,
    scope TEXT NOT NULL,
    non_browser INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    last_used_at TEXT
);

CREATE INDEX IF NOT EXISTS auth_origin ON auth (origin);
