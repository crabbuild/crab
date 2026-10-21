CREATE TABLE IF NOT EXISTS discussion_index_revision (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision BETWEEN 0 AND 9007199254740991)
) STRICT;
INSERT INTO discussion_index_revision(singleton, revision) VALUES (1, 0)
ON CONFLICT(singleton) DO NOTHING;

CREATE TABLE IF NOT EXISTS discussion_index (
    rowid INTEGER PRIMARY KEY,
    kind INTEGER NOT NULL CHECK (kind IN (0, 1)),
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 9007199254740991),
    state INTEGER NOT NULL CHECK (state BETWEEN 0 AND 2 AND (kind = 1 OR state < 2)),
    draft INTEGER NOT NULL CHECK (draft IN (0, 1) AND (kind = 1 OR draft = 0)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    author TEXT NOT NULL,
    comments INTEGER NOT NULL CHECK (comments BETWEEN 0 AND 9007199254740991),
    reactions INTEGER NOT NULL CHECK (reactions BETWEEN 0 AND 9007199254740991),
    UNIQUE (kind, number),
    CHECK (rowid = number * 2 + kind)
) STRICT;

CREATE INDEX IF NOT EXISTS discussion_index_created ON discussion_index(kind, created_at_ms, number);
CREATE INDEX IF NOT EXISTS discussion_index_updated ON discussion_index(kind, updated_at_ms, number);
CREATE INDEX IF NOT EXISTS discussion_index_comments ON discussion_index(kind, comments, number);
CREATE INDEX IF NOT EXISTS discussion_index_reactions ON discussion_index(kind, reactions, number);
CREATE INDEX IF NOT EXISTS discussion_index_states ON discussion_index(kind, state, number);

CREATE VIRTUAL TABLE IF NOT EXISTS discussion_fts USING fts5(
    title, body, author, content='', contentless_delete=1,
    tokenize='trigram case_sensitive 1'
);

CREATE TABLE IF NOT EXISTS discussion_index_labels (
    document INTEGER NOT NULL REFERENCES discussion_index(rowid) ON DELETE CASCADE,
    label INTEGER NOT NULL REFERENCES repository_labels(number),
    PRIMARY KEY (document, label)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS discussion_index_label_lookup ON discussion_index_labels(label, document);

CREATE TABLE IF NOT EXISTS discussion_index_assignees (
    document INTEGER NOT NULL REFERENCES discussion_index(rowid) ON DELETE CASCADE,
    subject TEXT NOT NULL,
    PRIMARY KEY (document, subject)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS discussion_index_assignee_lookup ON discussion_index_assignees(subject, document);

CREATE TABLE IF NOT EXISTS discussion_index_reaction_counts (
    document INTEGER NOT NULL REFERENCES discussion_index(rowid) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('thumbs_up','thumbs_down','laugh','hooray','confused','heart','rocket','eyes')),
    count INTEGER NOT NULL CHECK (count BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY (document, kind)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS discussion_index_reaction_lookup ON discussion_index_reaction_counts(kind, count, document);

CREATE TABLE IF NOT EXISTS discussion_index_dirty (
    kind INTEGER NOT NULL CHECK (kind IN (0, 1)),
    number INTEGER NOT NULL CHECK (number BETWEEN 1 AND 9007199254740991),
    PRIMARY KEY (kind, number)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER IF NOT EXISTS discussion_issue_insert AFTER INSERT ON repository_issues BEGIN
    INSERT INTO discussion_index_dirty VALUES (0, NEW.number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_issue_update AFTER UPDATE ON repository_issues BEGIN
    INSERT INTO discussion_index_dirty VALUES (0, NEW.number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_issue_delete AFTER DELETE ON repository_issues BEGIN
    INSERT INTO discussion_index_dirty VALUES (0, OLD.number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_pull_insert AFTER INSERT ON repository_pulls BEGIN
    INSERT INTO discussion_index_dirty VALUES (1, NEW.number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_pull_update AFTER UPDATE ON repository_pulls BEGIN
    INSERT INTO discussion_index_dirty VALUES (1, NEW.number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_pull_delete AFTER DELETE ON repository_pulls BEGIN
    INSERT INTO discussion_index_dirty VALUES (1, OLD.number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_issue_comment_insert AFTER INSERT ON repository_issue_comments BEGIN
    INSERT INTO discussion_index_dirty VALUES (0, NEW.issue_number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_issue_comment_update AFTER UPDATE ON repository_issue_comments BEGIN
    INSERT INTO discussion_index_dirty VALUES (0, NEW.issue_number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_issue_comment_delete AFTER DELETE ON repository_issue_comments BEGIN
    INSERT INTO discussion_index_dirty VALUES (0, OLD.issue_number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_pull_comment_insert AFTER INSERT ON repository_pull_comments BEGIN
    INSERT INTO discussion_index_dirty VALUES (1, NEW.pull_number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_pull_comment_update AFTER UPDATE ON repository_pull_comments BEGIN
    INSERT INTO discussion_index_dirty VALUES (1, NEW.pull_number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_pull_comment_delete AFTER DELETE ON repository_pull_comments BEGIN
    INSERT INTO discussion_index_dirty VALUES (1, OLD.pull_number) ON CONFLICT DO NOTHING;
END;
CREATE TRIGGER IF NOT EXISTS discussion_label_insert AFTER INSERT ON repository_labels BEGIN
    UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER IF NOT EXISTS discussion_label_update AFTER UPDATE ON repository_labels BEGIN
    UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER IF NOT EXISTS discussion_label_delete AFTER DELETE ON repository_labels BEGIN
    UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1;
END;
