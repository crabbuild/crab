use super::*;
use crate::{
    cells::{MutationIdentity, RepositoryCell, repository::*},
    server::Server,
    test_support::server as fixtures,
};
use serde_json::{Value, json};

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
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap()
}
async fn create(
    cell: &RepositoryCell,
    kind: SubjectKind,
    id: [u8; 16],
    metadata: Input,
) -> Result<Value> {
    match kind {
        SubjectKind::Issue => serde_json::to_value(
            cell.client
                .command::<CreateIssue>(
                    &cell.target,
                    identity(),
                    CreateIssueInput {
                        metadata,
                        submission_id: id,
                        author: actor(),
                        title: "Initial selection".into(),
                        body: "Native creation".into(),
                    },
                )
                .await?
                .output,
        )
        .map_err(Error::sdk),
        SubjectKind::Pull => serde_json::to_value(
            cell.client
                .command::<CreatePull>(
                    &cell.target,
                    identity(),
                    CreatePullInput {
                        metadata,
                        draft: false,
                        submission_id: id,
                        author: actor(),
                        title: "Initial selection".into(),
                        body: "Native creation".into(),
                        base_ref: "refs/heads/main".into(),
                        base_oid: "a".repeat(40),
                        head_ref: "refs/heads/feature".into(),
                        head_oid: "b".repeat(40),
                    },
                )
                .await?
                .output,
        )
        .map_err(Error::sdk),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_selections_and_rejections_are_consistent_for_both_discussion_kinds() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    cell.client
        .command::<CreateLabel>(
            &cell.target,
            identity(),
            CreateLabelInput {
                author: actor(),
                submission_id: *Uuid::now_v7().as_bytes(),
                name: "bug".into(),
                color: "1d76db".into(),
                description: None,
            },
        )
        .await
        .unwrap();
    cell.client
        .command::<milestones::CreateMilestone>(
            &cell.target,
            identity(),
            milestones::CreateMilestoneInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: "Release".into(),
                description: String::new(),
                due_on: None,
            },
        )
        .await
        .unwrap();
    let person = Assignee {
        id: Uuid::now_v7(),
        name: "Bob".into(),
    };
    let metadata = Input {
        can_triage: true,
        selection: Selection {
            label_ids: vec![1],
            assignee_ids: vec![person.id],
            milestone: Some(1),
        },
        eligible: vec![person.clone()],
    };
    for kind in [SubjectKind::Issue, SubjectKind::Pull] {
        let request = *Uuid::now_v7().as_bytes();
        let mut denied = metadata.clone();
        denied.can_triage = false;
        let rejected = create(&cell, kind, request, denied).await.unwrap();
        assert_eq!(rejected, json!({"MetadataRejected":"forbidden"}));
        if kind == SubjectKind::Pull {
            let comparison = cell
                .client
                .query::<GetPullSubmission>(
                    &cell.target,
                    None,
                    PullSubmissionKey {
                        pull: None,
                        submission_id: request,
                    },
                )
                .await
                .unwrap()
                .output
                .unwrap();
            assert_eq!(comparison.base_oid, "a".repeat(40));
            assert_eq!(comparison.head_oid, "b".repeat(40));
        }
        assert_eq!(
            create(&cell, kind, request, metadata.clone())
                .await
                .unwrap(),
            rejected
        );
        let created = create(&cell, kind, *Uuid::now_v7().as_bytes(), metadata.clone())
            .await
            .unwrap();
        assert_eq!(created["Created"]["number"], 1);
        assert_eq!(created["Created"]["version"], 1);
        assert_eq!(created["Created"]["label_ids"], json!([1]));
        assert_eq!(created["Created"]["assignees"][0]["id"], json!(person.id));
    }
    let milestone = cell
        .client
        .query::<milestones::GetMilestone>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(milestone.open_count, 2);
    fixtures::close(&server).await;
}

pub(in crate::cells::repository) struct ExhaustProgress;
impl Mutation for ExhaustProgress {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 65_513;
    type Input = u64;
    type Output = ();
    fn execute(context: &mut Transaction<'_>, count: u64) -> Result<MutationResult<()>> {
        let mut milestone = milestones::GetMilestone::execute(context, 1)?.unwrap();
        milestone.open_count = count;
        context.put_document("collaboration/v1/milestones/0000000000000001", &milestone)?;
        Ok(MutationResult::Success(()))
    }
}

#[test]
fn metadata_identity_uses_actor_ids_and_retains_the_empty_submission_digest() {
    let original = CreateIssueInput {
        metadata: Input::default(),
        submission_id: [1; 16],
        author: actor(),
        title: "Original".into(),
        body: "Body".into(),
    };
    let mut historical = blake3::Hasher::new();
    historical.update(b"crab.repository.issue-submission.v1\0");
    for value in [
        &original.author.issuer,
        &original.author.subject,
        &original.title,
        &original.body,
    ] {
        historical.update(&(value.len() as u64).to_be_bytes());
        historical.update(value.as_bytes());
    }
    assert_eq!(
        crate::cells::repository::issue_submission_digest(&original),
        historical.finalize()
    );
    let mut selected = original.clone();
    selected
        .metadata
        .selection
        .assignee_ids
        .push(Uuid::now_v7());
    assert_ne!(
        crate::cells::repository::issue_submission_digest(&original),
        crate::cells::repository::issue_submission_digest(&selected)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_initial_milestone_write_rolls_back_discussion_number_and_reservation() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    cell.client
        .command::<milestones::CreateMilestone>(
            &cell.target,
            identity(),
            milestones::CreateMilestoneInput {
                submission_id: *Uuid::now_v7().as_bytes(),
                author: actor(),
                title: "Release".into(),
                description: String::new(),
                due_on: None,
            },
        )
        .await
        .unwrap();
    for kind in [SubjectKind::Issue, SubjectKind::Pull] {
        cell.client
            .command::<ExhaustProgress>(
                &cell.target,
                identity(),
                crate::cells::repository::MAX_NUMBER,
            )
            .await
            .unwrap();
        let id = *Uuid::now_v7().as_bytes();
        let metadata = Input {
            can_triage: true,
            selection: Selection {
                milestone: Some(1),
                ..Default::default()
            },
            eligible: vec![],
        };
        assert!(create(&cell, kind, id, metadata.clone()).await.is_err());
        cell.client
            .command::<ExhaustProgress>(&cell.target, identity(), 0)
            .await
            .unwrap();
        let saved = create(&cell, kind, id, metadata).await.unwrap();
        assert_eq!(saved["Created"]["number"], 1);
        assert_eq!(saved["Created"]["version"], 1);
    }
    fixtures::close(&server).await;
}
