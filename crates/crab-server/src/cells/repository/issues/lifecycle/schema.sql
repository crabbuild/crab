CREATE TABLE repository_issue_lifecycle (
    issue INTEGER PRIMARY KEY,
    state_reason TEXT CHECK (state_reason IN ('completed', 'not_planned', 'reopened')),
    closed_at_ms INTEGER CHECK (closed_at_ms >= 0),
    CHECK (state_reason <> 'reopened' OR closed_at_ms IS NULL),
    FOREIGN KEY (issue) REFERENCES repository_issues(number) ON DELETE CASCADE
) STRICT;
CREATE INDEX repository_issue_reasons ON repository_issue_lifecycle(state_reason, issue);
