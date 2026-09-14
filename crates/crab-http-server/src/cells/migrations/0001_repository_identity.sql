CREATE TABLE repository_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    repository_uuid BLOB NOT NULL UNIQUE CHECK (length(repository_uuid) = 16),
    app_revision INTEGER NOT NULL DEFAULT 0
        CHECK (app_revision BETWEEN 0 AND 9007199254740991)
) STRICT;
