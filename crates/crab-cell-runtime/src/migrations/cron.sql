CREATE TABLE cron_schedules (
    schedule_id BLOB PRIMARY KEY CHECK (length(schedule_id) = 16),
    target_index INTEGER NOT NULL CHECK (target_index >= 0),
    target_partition BLOB NOT NULL CHECK (length(target_partition) <= 1024),
    payload BLOB NOT NULL CHECK (length(payload) <= 262144),
    interval_ms INTEGER NOT NULL CHECK (interval_ms BETWEEN 1000 AND 31536000000),
    next_due_ms INTEGER NOT NULL CHECK (next_due_ms >= 0),
    occurrence INTEGER NOT NULL CHECK (occurrence >= 0),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    generation INTEGER NOT NULL CHECK (generation >= 1),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT, WITHOUT ROWID;
CREATE INDEX cron_due ON cron_schedules(enabled, next_due_ms, schedule_id);
