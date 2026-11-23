-- Tokens can be revoked without deleting their audit row. A revoked token is
-- ignored by handshakes but keeps its id, origin, scopes and creation time.
ALTER TABLE auth ADD COLUMN revoked_at TEXT;
