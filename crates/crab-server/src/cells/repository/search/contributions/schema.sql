CREATE TABLE IF NOT EXISTS discussion_index_commenters (
    source INTEGER NOT NULL CHECK(source BETWEEN 0 AND 4),
    kind INTEGER NOT NULL CHECK(kind = (source <> 0)),
    discussion INTEGER NOT NULL CHECK(discussion BETWEEN 1 AND 9007199254740991),
    thread INTEGER NOT NULL CHECK((source = 4 AND thread BETWEEN 1 AND 9007199254740991) OR (source <> 4 AND thread = 0)),
    number INTEGER NOT NULL CHECK(number BETWEEN 1 AND 9007199254740991),
    identity BLOB NOT NULL CHECK(length(identity) = 32),
    PRIMARY KEY(source, discussion, thread, number)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS discussion_index_commenter_lookup ON discussion_index_commenters(identity, kind, discussion);

CREATE TRIGGER IF NOT EXISTS discussion_commenter_insert AFTER INSERT ON discussion_index_commenters BEGIN
    UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER IF NOT EXISTS discussion_commenter_delete AFTER DELETE ON discussion_index_commenters BEGIN
    UPDATE discussion_index_revision SET revision = revision + 1 WHERE singleton = 1;
END;

-- Track each contribution separately so deleting one leaves others intact.
-- Source deletion triggers replace a polymorphic foreign key; disposable
-- discussion_index rows cannot own these relationships.
CREATE TRIGGER IF NOT EXISTS discussion_commenter_issue_delete AFTER DELETE ON repository_issue_comments BEGIN
    DELETE FROM discussion_index_commenters WHERE source = 0 AND discussion = OLD.issue_number AND thread = 0 AND number = OLD.number;
END;
CREATE TRIGGER IF NOT EXISTS discussion_commenter_pull_delete AFTER DELETE ON repository_pull_comments BEGIN
    DELETE FROM discussion_index_commenters WHERE source = 1 AND discussion = OLD.pull_number AND thread = 0 AND number = OLD.number;
END;
CREATE TRIGGER IF NOT EXISTS discussion_commenter_review_delete AFTER DELETE ON repository_pull_reviews BEGIN
    DELETE FROM discussion_index_commenters WHERE source = 2 AND discussion = OLD.pull_number AND thread = 0 AND number = OLD.number;
END;
CREATE TRIGGER IF NOT EXISTS discussion_commenter_thread_delete AFTER DELETE ON repository_pull_review_threads BEGIN
    DELETE FROM discussion_index_commenters WHERE source = 3 AND discussion = OLD.pull_number AND thread = 0 AND number = OLD.number;
END;
CREATE TRIGGER IF NOT EXISTS discussion_commenter_reply_delete AFTER DELETE ON repository_pull_review_replies BEGIN
    DELETE FROM discussion_index_commenters WHERE source = 4 AND discussion = OLD.pull_number AND thread = OLD.thread_number AND number = OLD.number;
END;
