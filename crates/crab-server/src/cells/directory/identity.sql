CREATE TABLE directory_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    format INTEGER NOT NULL CHECK (format = 2),
    app_revision INTEGER NOT NULL DEFAULT 0 CHECK (app_revision BETWEEN 0 AND 9007199254740991)
) STRICT;
INSERT INTO directory_identity(singleton, format) VALUES (1, 2);
