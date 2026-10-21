use super::{Decision, Subject, SubjectKind, milestones::*};
use crate::cells::repository::{
    CreateIssue, CreateIssueInput, CreateIssueOutcome, RepositoryAuthor, UpdateIssue,
    UpdateIssueInput, UpdateIssueOutcome,
};
use crate::cells::{MutationIdentity, RepositoryCell};
use crate::server::Server;
use crate::test_support::server as fixtures;
use uuid::Uuid;

mod milestones;

fn actor() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    }
}

fn identity() -> MutationIdentity {
    let now = crate::cells::unix_now_ms().unwrap();
    MutationIdentity {
        request_id: Uuid::now_v7(),
        issued_at_ms: now,
        expires_at_ms: now + 60_000,
    }
}

fn cell(server: &Server) -> RepositoryCell {
    server
        .repository_cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap()
}

fn input(title: &str) -> CreateMilestoneInput {
    CreateMilestoneInput {
        submission_id: *Uuid::now_v7().as_bytes(),
        author: actor(),
        title: title.into(),
        description: "Release planning".into(),
        due_on: Some("2028-02-29".into()),
    }
}

fn applied<T>(decision: Decision<T>) -> T {
    match decision {
        Decision::Applied(value) => value,
        _ => panic!("collaboration command was rejected"),
    }
}
