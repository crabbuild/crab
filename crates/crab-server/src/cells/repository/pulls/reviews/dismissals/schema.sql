CREATE TABLE repository_pull_review_dismissals (
    pull_number INTEGER NOT NULL,
    review_number INTEGER NOT NULL,
    actor_issuer TEXT NOT NULL,
    actor_subject TEXT NOT NULL,
    actor_name TEXT NOT NULL,
    body TEXT NOT NULL,
    version INTEGER NOT NULL CHECK(version > 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY(pull_number, review_number),
    FOREIGN KEY(pull_number, review_number) REFERENCES repository_pull_reviews(pull_number, number)
) STRICT;
