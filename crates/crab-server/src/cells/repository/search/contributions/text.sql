CREATE TABLE IF NOT EXISTS discussion_index_comment_text (
    id INTEGER PRIMARY KEY,
    source INTEGER NOT NULL,
    kind INTEGER NOT NULL CHECK(kind = (source <> 0)),
    discussion INTEGER NOT NULL,
    thread INTEGER NOT NULL,
    number INTEGER NOT NULL,
    body TEXT NOT NULL,
    UNIQUE(source, discussion, thread, number),
    FOREIGN KEY(source, discussion, thread, number)
        REFERENCES discussion_index_commenters(source, discussion, thread, number) ON DELETE CASCADE
) STRICT;
CREATE INDEX IF NOT EXISTS discussion_index_comment_text_parent ON discussion_index_comment_text(kind, discussion, id);
CREATE VIRTUAL TABLE IF NOT EXISTS discussion_comment_fts USING fts5(
    body, content='', contentless_delete=1, tokenize='trigram case_sensitive 1'
);
CREATE TABLE IF NOT EXISTS discussion_index_comment_text_dirty (
    id INTEGER PRIMARY KEY
) STRICT;

-- Native flush maintains FTS in the domain transaction. SQLite's untrusted
-- schema must never execute virtual-table writes from persistent triggers.
CREATE TRIGGER IF NOT EXISTS discussion_comment_text_insert AFTER INSERT ON discussion_index_comment_text BEGIN
    INSERT INTO discussion_index_comment_text_dirty VALUES (NEW.id) ON CONFLICT DO NOTHING;
    UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER IF NOT EXISTS discussion_comment_text_update AFTER UPDATE OF body ON discussion_index_comment_text BEGIN
    INSERT INTO discussion_index_comment_text_dirty VALUES (NEW.id) ON CONFLICT DO NOTHING;
    UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER IF NOT EXISTS discussion_comment_text_delete AFTER DELETE ON discussion_index_comment_text BEGIN
    INSERT INTO discussion_index_comment_text_dirty VALUES (OLD.id) ON CONFLICT DO NOTHING;
    UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1;
END;
