CREATE TABLE repository_pull_lifecycle (
    pull INTEGER PRIMARY KEY REFERENCES repository_pulls(number) ON DELETE CASCADE,
    closed_at_ms INTEGER CHECK(closed_at_ms >= 0),
    merged_at_ms INTEGER CHECK(merged_at_ms >= 0),
    CHECK(merged_at_ms IS NULL OR merged_at_ms = closed_at_ms)
) STRICT;
