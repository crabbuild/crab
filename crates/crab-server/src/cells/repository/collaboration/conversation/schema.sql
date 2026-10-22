CREATE TABLE IF NOT EXISTS discussion_conversation (
    kind INTEGER NOT NULL CHECK(kind IN (0, 1)),
    discussion INTEGER NOT NULL CHECK(discussion BETWEEN 1 AND 9007199254740991),
    source INTEGER NOT NULL CHECK(source BETWEEN 0 AND 2 AND (source <> 1 OR kind = 1)),
    number INTEGER NOT NULL CHECK(number BETWEEN 1 AND 9007199254740991),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms BETWEEN 0 AND 9007199254740991),
    PRIMARY KEY(kind, discussion, source, number)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS discussion_conversation_order ON discussion_conversation(kind, discussion, created_at_ms, source, number);

CREATE TRIGGER IF NOT EXISTS conversation_repository_issue_comments_insert AFTER INSERT ON repository_issue_comments BEGIN
    INSERT INTO discussion_conversation(kind, discussion, source, number, created_at_ms)
    VALUES (0, NEW.issue_number, 0, NEW.number, NEW.created_at_ms);
END;
CREATE TRIGGER IF NOT EXISTS conversation_repository_issue_comments_delete AFTER DELETE ON repository_issue_comments BEGIN
    DELETE FROM discussion_conversation WHERE kind = 0 AND discussion = OLD.issue_number AND source = 0 AND number = OLD.number;
END;

CREATE TRIGGER IF NOT EXISTS conversation_repository_pull_comments_insert AFTER INSERT ON repository_pull_comments BEGIN
    INSERT INTO discussion_conversation(kind, discussion, source, number, created_at_ms)
    VALUES (1, NEW.pull_number, 0, NEW.number, NEW.created_at_ms);
END;
CREATE TRIGGER IF NOT EXISTS conversation_repository_pull_comments_delete AFTER DELETE ON repository_pull_comments BEGIN
    DELETE FROM discussion_conversation WHERE kind = 1 AND discussion = OLD.pull_number AND source = 0 AND number = OLD.number;
END;

CREATE TRIGGER IF NOT EXISTS conversation_repository_pull_reviews_insert AFTER INSERT ON repository_pull_reviews BEGIN
    INSERT INTO discussion_conversation(kind, discussion, source, number, created_at_ms)
    VALUES (1, NEW.pull_number, 1, NEW.number, NEW.created_at_ms);
END;
CREATE TRIGGER IF NOT EXISTS conversation_repository_pull_reviews_delete AFTER DELETE ON repository_pull_reviews BEGIN
    DELETE FROM discussion_conversation WHERE kind = 1 AND discussion = OLD.pull_number AND source = 1 AND number = OLD.number;
END;

CREATE TRIGGER IF NOT EXISTS conversation_repository_issues_delete AFTER DELETE ON repository_issues BEGIN
    DELETE FROM discussion_conversation WHERE kind = 0 AND discussion = OLD.number;
END;

CREATE TRIGGER IF NOT EXISTS conversation_repository_pulls_delete AFTER DELETE ON repository_pulls BEGIN
    DELETE FROM discussion_conversation WHERE kind = 1 AND discussion = OLD.number;
END;
