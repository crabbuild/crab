use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::client::{Committed, InvocationError};
use crab_cell_runtime::identity::RequestId;
use futures_util::future::join_all;
use uuid::Uuid;

use crate::auth::Identity;
use crate::cells::RepositoryCellRouter;
use crate::cells::repository::{
    CreateLabel, CreateLabelInput, CreateLabelOutcome, GetRepositoryLifecycle, ListLabels,
    ReplaceRepositoryLifecycle, ReplaceRepositoryLifecycleInput, ReplaceRepositoryLifecycleOutcome,
    RepositoryAuthor,
};

pub(super) struct RecordedLabel {
    identity: MutationIdentity,
    input: CreateLabelInput,
    committed: Committed<CreateLabelOutcome>,
}

pub(super) async fn run(
    router: &RepositoryCellRouter,
    repository: Uuid,
    principal: &Identity,
) -> Vec<RecordedLabel> {
    let labels = router
        .route(repository, principal, "repository.label.create")
        .await
        .unwrap();
    let lifecycle = router
        .route(repository, principal, "repository.settings.lifecycle")
        .await
        .unwrap();
    let reader = router
        .route(repository, principal, "repository.read")
        .await
        .unwrap();
    let mut version = reader
        .client
        .query::<GetRepositoryLifecycle>(&reader.target, None, ())
        .await
        .unwrap()
        .output
        .version;
    let mut recorded = Vec::new();
    for round in 0..4 {
        let inputs: Vec<_> = (0..6)
            .map(|index| {
                let identity = mutation();
                let input = CreateLabelInput {
                    submission_id: *identity.request_id.as_bytes(),
                    author: RepositoryAuthor {
                        issuer: principal.issuer.clone(),
                        subject: principal.subject.clone(),
                        name: principal.name.clone(),
                    },
                    name: format!("archive-race-{round}-{index}"),
                    color: "123456".into(),
                    description: None,
                };
                (identity, input)
            })
            .collect();
        let before = outcome(
            labels
                .client
                .command::<CreateLabel>(&labels.target, inputs[0].0, inputs[0].1.clone())
                .await,
        );
        assert!(matches!(before.output, CreateLabelOutcome::Created(_)));

        // Receipts establish order even when peer replies arrive out of order.
        // Duplicate delivery must retain that same decision and commit sequence.
        let submissions = join_all(inputs[1..5].iter().map(|(identity, input)| {
            let labels = &labels;
            async move {
                let (first, duplicate) = tokio::join!(
                    labels
                        .client
                        .command::<CreateLabel>(&labels.target, *identity, input.clone()),
                    labels
                        .client
                        .command::<CreateLabel>(&labels.target, *identity, input.clone()),
                );
                let first = outcome(first);
                assert_eq!(first, outcome(duplicate));
                first
            }
        }));
        let (archived, concurrent) = tokio::join!(
            lifecycle.client.command::<ReplaceRepositoryLifecycle>(
                &lifecycle.target,
                mutation(),
                ReplaceRepositoryLifecycleInput {
                    expected_version: version,
                    archived: true,
                },
            ),
            submissions,
        );
        let archived = archived.unwrap();
        let ReplaceRepositoryLifecycleOutcome::Updated(state) = archived.output else {
            panic!("concurrent archive did not commit");
        };
        version = state.version;
        let after = outcome(
            labels
                .client
                .command::<CreateLabel>(&labels.target, inputs[5].0, inputs[5].1.clone())
                .await,
        );
        assert_eq!(after.output, CreateLabelOutcome::Archived);
        let outcomes = std::iter::once(before)
            .chain(concurrent)
            .chain(std::iter::once(after));
        for ((identity, input), committed) in inputs.into_iter().zip(outcomes) {
            let precedes_archive =
                committed.receipt.commit_sequence < archived.receipt.commit_sequence;
            assert_eq!(
                matches!(committed.output, CreateLabelOutcome::Created(_)),
                precedes_archive,
                "mutation decision must match its durable position relative to archive",
            );
            recorded.push(RecordedLabel {
                identity,
                input,
                committed,
            });
        }
        let unarchived = lifecycle
            .client
            .command::<ReplaceRepositoryLifecycle>(
                &lifecycle.target,
                mutation(),
                ReplaceRepositoryLifecycleInput {
                    expected_version: version,
                    archived: false,
                },
            )
            .await
            .unwrap();
        let ReplaceRepositoryLifecycleOutcome::Updated(state) = unarchived.output else {
            panic!("unarchive did not commit");
        };
        version = state.version;
    }
    recorded
}

pub(super) async fn verify_recovered(
    router: &RepositoryCellRouter,
    repository: Uuid,
    principal: &Identity,
    recorded: &[RecordedLabel],
) {
    let labels = router
        .route(repository, principal, "repository.label.create")
        .await
        .unwrap();
    for record in recorded {
        let replay = labels
            .client
            .command::<CreateLabel>(&labels.target, record.identity, record.input.clone())
            .await;
        assert_eq!(outcome(replay), record.committed);
    }
    let reader = router
        .route(repository, principal, "repository.read")
        .await
        .unwrap();
    let catalog = reader
        .client
        .query::<ListLabels>(&reader.target, None, ())
        .await
        .unwrap()
        .output;
    for record in recorded {
        let found = catalog
            .labels
            .iter()
            .find(|label| label.name == record.input.name);
        match &record.committed.output {
            CreateLabelOutcome::Created(label) => assert_eq!(found, Some(label)),
            CreateLabelOutcome::Archived => assert!(found.is_none()),
            other => panic!("unexpected archive race result: {other:?}"),
        }
    }
    let created = recorded
        .iter()
        .filter(|record| matches!(record.committed.output, CreateLabelOutcome::Created(_)))
        .count();
    eprintln!(
        "qualified archive race: {} recorded outcomes, {created} created, {} rejected; duplicate and recovered receipts unchanged",
        recorded.len(),
        recorded.len() - created
    );
}

fn outcome(
    result: Result<Committed<CreateLabelOutcome>, InvocationError<CreateLabelOutcome>>,
) -> Committed<CreateLabelOutcome> {
    match result {
        Ok(committed) if matches!(committed.output, CreateLabelOutcome::Created(_)) => committed,
        Err(InvocationError::Rejected(committed))
            if committed.output == CreateLabelOutcome::Archived =>
        {
            *committed
        }
        other => panic!("archive race did not return an authoritative outcome: {other:?}"),
    }
}

fn mutation() -> MutationIdentity {
    let now_ms = crate::cells::unix_now_ms().unwrap();
    MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}
