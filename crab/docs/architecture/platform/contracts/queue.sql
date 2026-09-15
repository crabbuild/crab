-- Queue schema version 1. state: ready=0, leased=1, acked=2, dead=3.
CREATE TABLE queue_messages (
    message_id BLOB PRIMARY KEY CHECK (length(message_id) = 16),
    payload BLOB NOT NULL CHECK (length(payload) <= 262144),
    state INTEGER NOT NULL CHECK (state BETWEEN 0 AND 3),
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    due_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    token BLOB,
    lease_until_ms INTEGER,
    result_code INTEGER,
    dead_letter_effect_id BLOB CHECK (dead_letter_effect_id IS NULL OR length(dead_letter_effect_id) = 32),
    CHECK ((state = 1 AND token IS NOT NULL AND length(token) = 16 AND lease_until_ms IS NOT NULL)
        OR (state != 1 AND token IS NULL AND lease_until_ms IS NULL)),
    CHECK (dead_letter_effect_id IS NULL OR state = 3)
) STRICT, WITHOUT ROWID;
CREATE INDEX queue_ready ON queue_messages(state, due_at_ms, message_id);
CREATE INDEX queue_leases ON queue_messages(state, lease_until_ms, message_id);
CREATE INDEX queue_retention ON queue_messages(expires_at_ms);

CREATE TABLE queue_dedup (
    producer_id BLOB PRIMARY KEY CHECK (length(producer_id) = 16),
    payload_digest BLOB NOT NULL CHECK (length(payload_digest) = 32),
    message_id BLOB NOT NULL CHECK (length(message_id) = 16),
    retain_until_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX queue_dedup_expiry ON queue_dedup(retain_until_ms);
