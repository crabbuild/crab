use super::*;
use crate::cells::repository::{
    CreateIssue, CreateIssueInput, CreateIssueOutcome, GetIssue, RepositoryAuthor,
};
use crate::cells::{RepositoryCell, application::Mutation};
use crate::test_support::server as fixtures;
use uuid::Uuid;

pub(in crate::cells::repository) struct SeedSubjects;
impl Mutation for SeedSubjects {
    const CLASS: &'static str = super::super::super::CLASS;
    const ID: u32 = 65_530;
    type Input = (u64, bool);
    type Output = ();
    fn execute(
        context: &mut Transaction<'_>,
        (count, abort_update): Self::Input,
    ) -> Result<MutationResult<()>> {
        for number in 1..=count {
            let outcome = CreateIssue::execute(
                context,
                CreateIssueInput {
                    submission_id: *Uuid::from_u128(number.into()).as_bytes(),
                    author: RepositoryAuthor {
                        issuer: "https://identity.test".into(),
                        subject: "alice".into(),
                        name: "Alice".into(),
                    },
                    title: format!("Report {number}"),
                    body: "Stored content".into(),
                },
            )?;
            if !matches!(
                outcome,
                MutationResult::Success(CreateIssueOutcome::Created(_))
            ) {
                return Err(Error::Invalid("Failed to seed assignment source"));
            }
        }
        for table in ["repository_issues", "repository_pulls"] {
            context.install_schema(&format!(
                "ALTER TABLE {table} RENAME COLUMN assignees TO assignee_subjects;"
            ))?;
            context.query(
                &format!("UPDATE {table} SET assignee_subjects = ?"),
                &[SqlValue::Blob(vec![
                    0, 0, 0, 1, 0, 0, 0, 5, b'a', b'l', b'i', b'c', b'e',
                ])],
            )?;
        }
        if abort_update {
            context.install_schema("CREATE TRIGGER reject_second_assignment BEFORE UPDATE ON repository_issues WHEN NEW.number = 2 BEGIN SELECT RAISE(ABORT, 'injected assignment update failure'); END;")?;
        }
        context.delete_document(FORMAT)?;
        let mut index: serde_json::Value = context
            .document("discussion-search/v1/state")?
            .ok_or(Error::Invalid("Missing test index"))?;
        context.install_schema(
            "DROP TABLE discussion_index_assignees; CREATE TABLE discussion_index_assignees (document INTEGER NOT NULL REFERENCES discussion_index(rowid) ON DELETE CASCADE, subject TEXT NOT NULL, PRIMARY KEY(document, subject)) STRICT, WITHOUT ROWID; CREATE INDEX discussion_index_assignee_lookup ON discussion_index_assignees(subject, document);",
        )?;
        index["format"] = serde_json::json!(3);
        context.put_document("discussion-search/v1/state", &index)?;
        Ok(MutationResult::Success(()))
    }
}
pub(in crate::cells::repository) struct InspectColumns;
impl Read for InspectColumns {
    const CLASS: &'static str = super::super::super::CLASS;
    const ID: u32 = 65_531;
    type Input = ();
    type Output = Vec<String>;
    fn execute(context: &mut Transaction<'_>, _: ()) -> Result<Self::Output> {
        let rows = context.query("PRAGMA table_info(repository_issues)", &[])?;
        rows.rows
            .iter()
            .map(|row| super::super::super::result_text(row, 1))
            .collect()
    }
}
fn cell(server: &crate::server::Server) -> RepositoryCell {
    server
        .cells()
        .unwrap()
        .repository(server.repositories.values()[0].id)
        .unwrap()
}
async fn write<M: Mutation>(cell: &RepositoryCell, input: M::Input) -> Result<M::Output> {
    cell.client
        .command::<M>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            input,
        )
        .await
        .map(|r| r.output)
}
#[test]
fn legacy_inventory_reads_the_original_length_prefixed_subject_fixture() {
    let stored = vec![0, 0, 0, 1, 0, 0, 0, 5, b'a', b'l', b'i', b'c', b'e'];
    assert_eq!(legacy_decode(&stored).unwrap(), vec!["alice".to_owned()]);
}
#[tokio::test]
async fn explicit_mapping_preserves_records_and_progress_across_native_frames() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    write::<SeedSubjects>(&cell, (17, false)).await.unwrap();
    assert!(matches!(
        cell.client.query::<GetIssue>(&cell.target, None, 1).await,
        Err(Error::SchemaUnavailable)
    ));
    let mut copied = 0;
    loop {
        let batch = cell
            .client
            .query::<GetAssignmentBatch>(&cell.target, None, None)
            .await
            .unwrap()
            .output;
        assert!(batch.records.len() <= 8);
        let version = match batch.state {
            AssignmentState::Outdated => 0,
            AssignmentState::Migrating {
                version,
                copied: seen,
            } => {
                assert_eq!(seen, copied);
                version
            }
            AssignmentState::Ready => break,
        };
        let records = batch
            .records
            .iter()
            .map(|record| RecordMapping {
                target: record.target,
                version: record.version,
                mappings: vec![AssignmentMapping {
                    subject: "alice".into(),
                    assignee: Assignee {
                        id: Uuid::from_u128(99),
                        name: "Historical Alice".into(),
                    },
                }],
            })
            .collect();
        copied += batch.records.len() as u64;
        write::<MigrateAssignments>(&cell, MigrateAssignmentsInput { version, records })
            .await
            .unwrap();
    }
    assert_eq!(copied, 17);
    let issue = cell
        .client
        .query::<GetIssue>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        (issue.title.as_str(), issue.body.as_str(), issue.version),
        ("Report 1", "Stored content", 1)
    );
    assert_eq!(
        issue.assignees,
        vec![Assignee {
            id: Uuid::from_u128(99),
            name: "Historical Alice".into()
        }]
    );
    use crate::cells::repository::search::index::{
        GetSearchIndexStatus, RebuildIndex, RebuildSearchInput, SearchIndexStatus,
    };
    let generation = Uuid::now_v7();
    write::<RebuildIndex>(
        &cell,
        RebuildSearchInput::Begin {
            expected: Some(cell.target),
            generation,
        },
    )
    .await
    .unwrap();
    loop {
        let status = cell
            .client
            .query::<GetSearchIndexStatus>(&cell.target, None, ())
            .await
            .unwrap()
            .output;
        let SearchIndexStatus::Building { progress, .. } = status else {
            assert!(matches!(status, SearchIndexStatus::Ready { .. }));
            break;
        };
        write::<RebuildIndex>(
            &cell,
            RebuildSearchInput::Step {
                generation,
                version: progress.version,
            },
        )
        .await
        .unwrap();
    }
    let retried = write::<CreateIssue>(
        &cell,
        CreateIssueInput {
            submission_id: *Uuid::from_u128(1).as_bytes(),
            author: RepositoryAuthor {
                issuer: "https://identity.test".into(),
                subject: "alice".into(),
                name: "Alice".into(),
            },
            title: "Report 1".into(),
            body: "Stored content".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(retried, CreateIssueOutcome::Created(Box::new(issue)));
    let columns = cell
        .client
        .query::<InspectColumns>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    assert!(
        columns.contains(&"assignees".into()) && !columns.contains(&"assignee_subjects".into())
    );
    fixtures::close(&server).await;
}
#[tokio::test]
async fn missing_mapping_leaves_source_schema_and_records_untouched() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    write::<SeedSubjects>(&cell, (1, false)).await.unwrap();
    assert!(
        write::<MigrateAssignments>(
            &cell,
            MigrateAssignmentsInput {
                version: 0,
                records: vec![]
            }
        )
        .await
        .is_err()
    );
    let columns = cell
        .client
        .query::<InspectColumns>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    assert!(
        columns.contains(&"assignee_subjects".into()) && !columns.contains(&"assignees".into())
    );
    assert_eq!(
        cell.client
            .query::<GetAssignmentBatch>(&cell.target, None, None)
            .await
            .unwrap()
            .output
            .state,
        AssignmentState::Outdated
    );
    fixtures::close(&server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn migration_receipt_and_cursor_survive_native_restart() {
    use crate::cells::runtime;
    use cellar::sdk::{NodeOutcome, ShutdownMode};
    async fn stop(node: cellar::sdk::Node) {
        let NodeOutcome::Stopped(report) = node.shutdown(ShutdownMode::Preserve).await.unwrap()
        else {
            panic!("Node did not stop")
        };
        assert!(
            report.cells_drained
                && report.ingress_drained
                && report.application_work_drained
                && !report.drain_timed_out
        );
    }
    let local = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:8788".parse().unwrap();
    let started = runtime::test_node(local.path(), address).await;
    let repository = Uuid::from_u128(123);
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    write::<SeedSubjects>(&cell, (9, false)).await.unwrap();
    let batch = cell
        .client
        .query::<GetAssignmentBatch>(&cell.target, None, None)
        .await
        .unwrap()
        .output;
    let input = MigrateAssignmentsInput {
        version: 0,
        records: batch
            .records
            .iter()
            .map(|record| RecordMapping {
                target: record.target,
                version: record.version,
                mappings: vec![AssignmentMapping {
                    subject: "alice".into(),
                    assignee: Assignee {
                        id: Uuid::from_u128(99),
                        name: "Alice".into(),
                    },
                }],
            })
            .collect(),
    };
    let request = crate::app::mutation_identity().unwrap();
    let first = cell
        .client
        .command::<MigrateAssignments>(&cell.target, request, input.clone())
        .await
        .unwrap();
    assert_eq!(
        first.output,
        MigrateAssignmentsOutcome::Progress {
            state: AssignmentState::Migrating {
                version: 1,
                copied: 8
            }
        }
    );
    stop(started.node).await;
    let restored = runtime::test_node(local.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    assert_eq!(
        cell.client
            .command::<MigrateAssignments>(&cell.target, request, input.clone())
            .await
            .unwrap(),
        first
    );
    let conflict_id = crate::app::mutation_identity().unwrap();
    let conflict = cell
        .client
        .command::<MigrateAssignments>(&cell.target, conflict_id, input.clone())
        .await
        .unwrap();
    assert_eq!(
        conflict.output,
        MigrateAssignmentsOutcome::Conflict {
            state: AssignmentState::Migrating {
                version: 1,
                copied: 8
            }
        }
    );
    assert!(matches!(
        cell.client
            .query::<GetAssignmentBatch>(
                &cell.target,
                None,
                Some(Subject {
                    kind: SubjectKind::Issue,
                    number: 7
                })
            )
            .await,
        Err(Error::Invalid(_))
    ));
    let batch = cell
        .client
        .query::<GetAssignmentBatch>(&cell.target, None, None)
        .await
        .unwrap()
        .output;
    assert_eq!(
        batch
            .records
            .iter()
            .map(|r| r.target.number)
            .collect::<Vec<_>>(),
        [9]
    );
    let records = batch
        .records
        .iter()
        .map(|record| RecordMapping {
            target: record.target,
            version: record.version,
            mappings: vec![AssignmentMapping {
                subject: "alice".into(),
                assignee: Assignee {
                    id: Uuid::from_u128(99),
                    name: "Alice".into(),
                },
            }],
        })
        .collect();
    write::<MigrateAssignments>(
        &cell,
        MigrateAssignmentsInput {
            version: 1,
            records,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        write::<MigrateAssignments>(
            &cell,
            MigrateAssignmentsInput {
                version: 2,
                records: vec![]
            }
        )
        .await
        .unwrap(),
        MigrateAssignmentsOutcome::Progress {
            state: AssignmentState::Ready
        }
    );
    stop(restored.node).await;
    let finished = runtime::test_node(local.path(), address).await;
    let cell = finished.client.repository(repository).unwrap();
    // Completion must not turn a recorded conflict into a successful new decision.
    assert_eq!(
        cell.client
            .command::<MigrateAssignments>(&cell.target, conflict_id, input)
            .await
            .unwrap(),
        conflict
    );
    stop(finished.node).await;
}

#[tokio::test]
async fn write_failure_rolls_back_schema_conversion_and_previous_rows() {
    let server = fixtures::fixture().await;
    let cell = cell(&server);
    write::<SeedSubjects>(&cell, (2, true)).await.unwrap();
    let before = cell
        .client
        .query::<GetAssignmentBatch>(&cell.target, None, None)
        .await
        .unwrap()
        .output;
    let records = before
        .records
        .iter()
        .map(|record| RecordMapping {
            target: record.target,
            version: record.version,
            mappings: vec![AssignmentMapping {
                subject: "alice".into(),
                assignee: Assignee {
                    id: Uuid::from_u128(99),
                    name: "Alice".into(),
                },
            }],
        })
        .collect();
    assert!(
        write::<MigrateAssignments>(
            &cell,
            MigrateAssignmentsInput {
                version: 0,
                records
            }
        )
        .await
        .is_err()
    );
    let columns = cell
        .client
        .query::<InspectColumns>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    assert!(
        columns.contains(&"assignee_subjects".into()) && !columns.contains(&"assignees".into())
    );
    let after = cell
        .client
        .query::<GetAssignmentBatch>(&cell.target, None, None)
        .await
        .unwrap()
        .output;
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    fixtures::close(&server).await;
}
