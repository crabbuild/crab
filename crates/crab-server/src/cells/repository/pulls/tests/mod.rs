use super::*;

#[test]
fn maximum_pull_decisions_fit_the_registered_output_bound() {
    let author = RepositoryAuthor {
        issuer: "🦀".repeat(512),
        subject: "🦀".repeat(512),
        name: "🦀".repeat(160),
    };
    let pull = PullRecord {
        draft: false,
        number: 1,
        create_submission_id: [8; 16],
        author: author.clone(),
        title: "t".repeat(256),
        body: "\u{1}".repeat(64 * 1024),
        state: PullState::Open,
        base_ref: "refs/heads/main".into(),
        base_oid: "0123456789abcdef0123456789abcdef01234567".into(),
        head_ref: "refs/heads/feature".into(),
        head_oid: "89abcdef0123456789abcdef0123456789abcdef".into(),
        label_ids: (1..=20).collect(),
        assignees: (1..=10)
            .map(|index| crate::cells::repository::assignments::Assignee {
                id: uuid::Uuid::from_u128(index),
                name: "🦀".repeat(160),
            })
            .collect(),
        merge_pending: None,
        merge: None,
        review_decisions: (1..=96)
            .map(|review| pulls::PullReviewDecision {
                review,
                author: author.clone(),
                state: ReviewState::Approved,
                commit_oid: "89abcdef0123456789abcdef0123456789abcdef".into(),
            })
            .collect(),
        version: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    assert!(serde_json::to_vec(&pull).unwrap().len() <= 1024 * 1024);
}

#[test]
fn maximum_pull_child_fits_the_registered_record_bound() {
    let comment = PullCommentRecord {
        pull: 1,
        number: 1,
        author: RepositoryAuthor {
            issuer: "🦀".repeat(512),
            subject: "🦀".repeat(512),
            name: "🦀".repeat(160),
        },
        body: "\u{1}".repeat(64 * 1024),
        version: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    assert!(serde_json::to_vec(&comment).unwrap().len() <= 512 * 1024);
}

mod lifecycle;
