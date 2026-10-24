use super::*;
use crate::{
    cells::{
        RepositoryCell,
        repository::{CreateIssue, CreateIssueInput, RepositoryAuthor},
    },
    test_support::server as fixtures,
};
use uuid::Uuid;

async fn write<M: Mutation>(cell: &RepositoryCell, input: M::Input) -> Result<M::Output> {
    Ok(cell
        .client
        .command::<M>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            input,
        )
        .await?
        .output)
}
async fn read<R: Read>(cell: &RepositoryCell, input: R::Input) -> R::Output {
    cell.client
        .query::<R>(&cell.target, None, input)
        .await
        .unwrap()
        .output
}
fn actor() -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: "https://identity.test".into(),
        subject: "alice".into(),
        name: "Alice".into(),
    }
}
#[derive(Clone)]
enum Command {
    Bulk(bulk::Input),
    Pins(pins::Input),
}
async fn run(cell: &RepositoryCell, command: &Command) -> Result<Value> {
    Ok(match command {
        Command::Bulk(input) => {
            serde_json::to_value(write::<bulk::Edit>(cell, input.clone()).await?)?
        }
        Command::Pins(input) => {
            serde_json::to_value(write::<pins::SetPins>(cell, input.clone()).await?)?
        }
    })
}
async fn history(cell: &RepositoryCell, kind: Kind) -> Vec<(Command, Value)> {
    let mut history = Vec::new();
    for index in 0..10 {
        let command = match kind {
            Kind::Bulk => Command::Bulk(bulk::Input {
                submission_id: *Uuid::now_v7().as_bytes(),
                actor: actor(),
                can_triage: true,
                items: vec![bulk::Selected {
                    item: bulk::Versioned {
                        subject: Subject {
                            kind: SubjectKind::Issue,
                            number: 1,
                        },
                        version: index + 1,
                    },
                    milestone_version: 0,
                }],
                action: bulk::Action::State {
                    state: if index % 2 == 0 {
                        state::State::Closed
                    } else {
                        state::State::Open
                    },
                    reason: None,
                },
                eligible: vec![],
            }),
            Kind::Pins => Command::Pins(pins::Input {
                submission_id: *Uuid::now_v7().as_bytes(),
                actor: actor(),
                can_write: true,
                version: index,
                numbers: if index % 2 == 0 { vec![1] } else { vec![] },
            }),
            Kind::Issue | Kind::Pull => panic!("state preparation is covered by its owner suite"),
        };
        let result = run(cell, &command).await.unwrap();
        history.push((command, result));
    }
    history
}
async fn seed(cell: &RepositoryCell) {
    write::<CreateIssue>(
        cell,
        CreateIssueInput {
            metadata: Default::default(),
            submission_id: *Uuid::now_v7().as_bytes(),
            author: actor(),
            title: "One".into(),
            body: "Body".into(),
        },
    )
    .await
    .unwrap();
}
pub(in crate::cells::repository) struct Historical;
impl Mutation for Historical {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 65_521;
    type Input = (Kind, bool);
    type Output = ();
    fn execute(
        context: &mut Transaction<'_>,
        (kind, corrupt): Self::Input,
    ) -> Result<MutationResult<()>> {
        let rows = context.documents::<Value>(kind.prefix(), None, 51)?;
        for (index, (key, value)) in rows.iter().enumerate() {
            let applied = value["resolution"]["Applied"]
                .as_u64()
                .ok_or(Error::Invalid(
                    "historical fixture requires applied decisions",
                ))?;
            let bad = corrupt && index == rows.len() - 1;
            let old = match kind {
                Kind::Bulk => {
                    serde_json::json!({"digest":value["digest"],"count":if bad {0} else {applied}})
                }
                Kind::Pins => {
                    serde_json::json!({"digest":value["digest"],"version":if bad {MAX_NUMBER+1} else {applied}})
                }
                Kind::Issue | Kind::Pull => {
                    return Err(Error::Invalid("unsupported historical fixture kind"));
                }
            };
            context.put_document(key, &old)?;
        }
        context.delete_document(super::key(kind))?;
        Ok(MutationResult::Success(()))
    }
}
pub(in crate::cells::repository) struct Inspect;
impl Read for Inspect {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 65_521;
    type Input = Kind;
    type Output = Vec<(String, Value)>;
    fn execute(context: &mut Transaction<'_>, kind: Kind) -> Result<Self::Output> {
        context.documents(kind.prefix(), None, 51)
    }
}
pub(in crate::cells::repository) struct Interrupted;
impl Mutation for Interrupted {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 65_522;
    type Input = Input;
    type Output = ();
    fn execute(context: &mut Transaction<'_>, input: Input) -> Result<MutationResult<()>> {
        Prepare::execute(context, input)?;
        Err(Error::Invalid("interrupted submission preparation"))
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn preparation_preserves_bulk_and_pin_results_history_and_replay_after_failed_frames() {
    use crate::cells::repository::collaboration::activity::{ListActivity, Query};
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for kind in [Kind::Bulk, Kind::Pins] {
        let commands = history(&cell, kind).await;
        let before = serde_json::to_value(
            read::<ListActivity>(
                &cell,
                Query {
                    subject: None,
                    before: None,
                    limit: 50,
                },
            )
            .await,
        )
        .unwrap();
        write::<Historical>(&cell, (kind, false)).await.unwrap();
        assert_eq!(
            read::<GetPreparation>(&cell, kind).await,
            LifecycleStatus::Outdated
        );
        assert!(matches!(
            run(&cell, &commands[0].0).await.err().unwrap(),
            Error::SchemaUnavailable
        ));
        assert!(matches!(
            write::<Prepare>(&cell, Input { kind, version: 0 })
                .await
                .unwrap(),
            PrepareLifecycleOutcome::Applied(LifecycleStatus::Migrating {
                version: 1,
                copied: 8
            })
        ));
        let staged = read::<Inspect>(&cell, kind).await;
        assert!(
            write::<Interrupted>(&cell, Input { kind, version: 1 })
                .await
                .is_err()
        );
        assert_eq!(read::<Inspect>(&cell, kind).await, staged);
        assert!(matches!(
            write::<Prepare>(&cell, Input { kind, version: 0 })
                .await
                .unwrap(),
            PrepareLifecycleOutcome::Conflict(_)
        ));
        assert!(matches!(
            write::<Prepare>(&cell, Input { kind, version: 1 })
                .await
                .unwrap(),
            PrepareLifecycleOutcome::Applied(LifecycleStatus::Ready)
        ));
        for (command, expected) in commands {
            assert_eq!(run(&cell, &command).await.unwrap(), expected);
        }
        assert_eq!(
            serde_json::to_value(
                read::<ListActivity>(
                    &cell,
                    Query {
                        subject: None,
                        before: None,
                        limit: 50
                    }
                )
                .await
            )
            .unwrap(),
            before
        );
    }
    fixtures::close(&server).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn invalid_historical_bulk_counts_and_pin_versions_do_not_advance_the_current_frame() {
    let server = fixtures::fixture().await;
    let cell = server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap();
    seed(&cell).await;
    for kind in [Kind::Bulk, Kind::Pins] {
        history(&cell, kind).await;
        write::<Historical>(&cell, (kind, true)).await.unwrap();
        assert!(matches!(
            write::<Prepare>(&cell, Input { kind, version: 0 })
                .await
                .unwrap(),
            PrepareLifecycleOutcome::Applied(LifecycleStatus::Migrating {
                version: 1,
                copied: 8
            })
        ));
        let staged = read::<Inspect>(&cell, kind).await;
        assert!(
            write::<Prepare>(&cell, Input { kind, version: 1 })
                .await
                .is_err()
        );
        assert_eq!(read::<Inspect>(&cell, kind).await, staged);
        assert_eq!(
            read::<GetPreparation>(&cell, kind).await,
            LifecycleStatus::Migrating {
                version: 1,
                copied: 8
            }
        );
    }
    fixtures::close(&server).await;
}
