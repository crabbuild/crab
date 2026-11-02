use super::*;
use crate::cells::repository::collaboration::lifecycle::{
    LifecycleStatus, PrepareLifecycleInput, PrepareLifecycleOutcome,
};
use crate::cells::repository::{RepositoryAuthor, projects, result_u64_from_row};
use crate::cells::{MutationIdentity, RepositoryCell, Result};
use cellar::sdk::{NodeOutcome, ShutdownMode};
use uuid::Uuid;

fn identity() -> MutationIdentity {
    crate::app::mutation_identity().unwrap()
}

fn source(n: u64) -> SourceIdentity {
    SourceIdentity {
        source_token: format!("source-{n}"),
        manifest_generation: n,
        manifest_etag: format!("etag-{n}"),
        journal_state_digest: format!("state-{n}"),
        pack_index_hash: "pack".into(),
        git_validation_digest: "validation".into(),
        commit_graph_hash: Some("graph".into()),
        path_state_hash: Some("paths".into()),
        head_ref: b"refs/heads/main".to_vec(),
    }
}

async fn apply(
    cell: &RepositoryCell,
    source: &SourceIdentity,
    operation: u8,
    epoch: u64,
    payload: Vec<u8>,
) -> ProjectionAck {
    cell.client
        .command::<ApplyProjectionBatch>(
            &cell.target,
            identity(),
            ProjectionBatch {
                operation,
                epoch_id: epoch,
                source: serde_json::to_vec(source).unwrap(),
                payload,
            },
        )
        .await
        .unwrap()
        .output
}

fn hash(n: u32) -> Vec<u8> {
    let mut hash = vec![0; 32];
    hash[..4].copy_from_slice(&n.to_be_bytes());
    hash
}

async fn build(cell: &RepositoryCell, n: u64) -> u64 {
    let source = source(n);
    let epoch = apply(cell, &source, OP_BEGIN, 0, vec![])
        .await
        .epoch_id
        .unwrap();
    let commits = vec![CommitRow {
        ordinal: 0,
        oid: vec![n as u8; 20],
        author_name: format!("Author {n}").into_bytes(),
        author_time: 0,
        message_preview: vec![],
    }];
    apply(
        cell,
        &source,
        OP_COMMITS,
        epoch,
        serde_json::to_vec(&commits).unwrap(),
    )
    .await;
    for start in (0..300).step_by(100) {
        let batch = AttributionBatch {
            nodes: (start..start + 100)
                .map(|n| AttributionNodeRow {
                    node_hash: hash(n),
                    last_change_ordinal: Some(0),
                    encoded_bytes: 0,
                })
                .collect(),
            edges: vec![],
            roots: vec![],
        };
        apply(
            cell,
            &source,
            OP_ATTRIBUTION,
            epoch,
            serde_json::to_vec(&batch).unwrap(),
        )
        .await;
    }
    for start in (1..300).step_by(100) {
        let batch = AttributionBatch {
            nodes: vec![],
            edges: (start..(start + 100).min(300))
                .map(|n| AttributionEdgeRow {
                    parent_hash: hash(0),
                    component: format!("file-{n}").into_bytes(),
                    child_hash: hash(n),
                })
                .collect(),
            roots: vec![],
        };
        apply(
            cell,
            &source,
            OP_ATTRIBUTION,
            epoch,
            serde_json::to_vec(&batch).unwrap(),
        )
        .await;
    }
    let roots = AttributionBatch {
        nodes: vec![],
        edges: vec![],
        roots: vec![AttributionRootRow {
            commit_ordinal: 0,
            root_hash: hash(0),
        }],
    };
    apply(
        cell,
        &source,
        OP_ATTRIBUTION,
        epoch,
        serde_json::to_vec(&roots).unwrap(),
    )
    .await;
    apply(cell, &source, OP_PROMOTE, epoch, vec![]).await;
    epoch
}

type Counts = Vec<(u64, String, [u64; 4])>;
pub(crate) struct Inspect;
impl Read for Inspect {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 65_501;
    type Input = ();
    type Output = Counts;
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Counts> {
        let mut result = vec![];
        for row in context
            .query(
                "SELECT epoch_id,state FROM git_projection_epochs ORDER BY epoch_id",
                &[],
            )?
            .rows
        {
            let epoch = optional_u64(&row, 0)?.unwrap();
            let mut counts = [0; 4];
            for (i, table) in [
                "git_attribution_roots",
                "git_epoch_commits",
                "git_attribution_edges",
                "git_attribution_nodes",
            ]
            .iter()
            .enumerate()
            {
                counts[i] = result_u64(
                    &context.sql(&SqlBatch {
                        statements: vec![statement(
                            &format!("SELECT COUNT(*) FROM {table} WHERE epoch_id=?"),
                            vec![integer(epoch)?],
                        )],
                    })?,
                    0,
                    0,
                )?;
            }
            result.push((epoch, result_text(&row, 1)?, counts));
        }
        Ok(result)
    }
}
async fn inspect(cell: &RepositoryCell) -> Counts {
    cell.client
        .query::<Inspect>(&cell.target, None, ())
        .await
        .unwrap()
        .output
}

async fn project(cell: &RepositoryCell) -> Uuid {
    let project = Uuid::now_v7();
    cell.client
        .command::<projects::Change>(
            &cell.target,
            identity(),
            projects::Request {
                request_id: Uuid::now_v7(),
                actor: RepositoryAuthor {
                    issuer: "https://identity.test".into(),
                    subject: "alice".into(),
                    name: "Alice".into(),
                },
                can_write: true,
                command: projects::Command::Create {
                    id: project,
                    name: "Canonical planning".into(),
                    template: "kanban".into(),
                },
                cancel: false,
            },
        )
        .await
        .unwrap();
    project
}
async fn planning(cell: &RepositoryCell, id: Uuid) -> projects::Project {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        cell.client
            .query::<projects::GetProject>(&cell.target, None, id),
    )
    .await
    .unwrap()
    .unwrap()
    .output
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn collection_batches_resume_after_restart_and_preserve_snapshot_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    let project = project(&cell).await;
    let before = planning(&cell, project).await;
    let oldest = build(&cell, 1).await;
    build(&cell, 2).await;
    let third = build(&cell, 3).await;
    // Reach the edge batch without mirroring the collector's phase order.
    loop {
        let previous = inspect(&cell).await;
        apply(&cell, &source(3), OP_COLLECT, 0, vec![]).await;
        let now = inspect(&cell).await;
        assert_eq!(planning(&cell, project).await, before);
        let old = now.iter().find(|row| row.0 == oldest).unwrap();
        if old.2[2] < 299 {
            let previous = previous.iter().find(|row| row.0 == oldest).unwrap();
            assert_eq!(previous.2[2] - old.2[2], 256);
            break;
        }
    }
    // Returning to the old immutable source allocates a fresh epoch; partial
    // reclamation must never be mistaken for a complete reusable snapshot.
    let returned = build(&cell, 1).await;
    assert!(returned > third);
    let checkpoint = inspect(&cell).await;
    let NodeOutcome::Stopped(report) = started.node.shutdown(ShutdownMode::Preserve).await.unwrap()
    else {
        panic!("node did not stop")
    };
    assert!(report.cells_drained && !report.drain_timed_out);
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    assert_eq!(inspect(&cell).await, checkpoint);
    let mut pending = Some(oldest);
    while pending.is_some() {
        let previous: u64 = inspect(&cell).await.iter().flat_map(|row| row.2).sum();
        pending = apply(&cell, &source(1), OP_COLLECT, 0, vec![])
            .await
            .epoch_id;
        let now: u64 = inspect(&cell).await.iter().flat_map(|row| row.2).sum();
        assert!(previous - now <= 256);
        assert_eq!(planning(&cell, project).await, before);
    }
    assert_eq!(
        inspect(&cell).await,
        vec![
            (third, "ready".into(), [1, 1, 299, 300]),
            (returned, "ready".into(), [1, 1, 299, 300])
        ]
    );
    apply(&cell, &source(3), OP_BEGIN, 0, vec![]).await;
    let query = AttributionQuery {
        source_token: source(3).source_token,
        commit_oid: vec![3; 20],
        paths: vec![b"file-299".to_vec()],
    };
    let reply = cell
        .client
        .query::<GetProjectionAttribution>(&cell.target, None, serde_json::to_vec(&query).unwrap())
        .await
        .unwrap()
        .output;
    let reply: AttributionResponse = serde_json::from_slice(&reply).unwrap();
    assert_eq!(reply.items[0].author, b"Author 3");
    restored
        .node
        .shutdown(ShutdownMode::Preserve)
        .await
        .unwrap();
}

pub(crate) struct Historical;
impl Mutation for Historical {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 65_500;
    type Input = u32;
    type Output = ();
    fn execute(context: &mut Transaction<'_>, nodes: u32) -> Result<MutationResult<()>> {
        for table in [
            "git_projection_state",
            "git_projection_refs",
            "git_attribution_roots",
            "git_attribution_edges",
            "git_attribution_nodes",
            "git_epoch_commits",
            "git_projection_epochs",
        ] {
            context.query(&format!("DROP TABLE {table}"), &[])?;
        }
        context.install_schema(include_str!("previous.sql"))?;
        context.delete_document(preparation::KEY)?;
        for n in 0..nodes {
            context.query(
                "INSERT INTO git_attribution_nodes(node_hash,encoded_bytes) VALUES(?,0)",
                &[SqlValue::Blob(hash(n))],
            )?;
            if n > 0 {
                context.query("INSERT INTO git_attribution_edges(parent_hash,component,child_hash) VALUES(?,?,?)", &[SqlValue::Blob(hash(0)), SqlValue::Blob(n.to_be_bytes().to_vec()), SqlValue::Blob(hash(n))])?;
            }
        }
        Ok(MutationResult::Success(()))
    }
}

pub(crate) struct InspectHistorical;
impl Read for InspectHistorical {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 65_502;
    type Input = ();
    type Output = [u64; 2];
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        let mut counts = [0; 2];
        for (i, table) in ["git_attribution_nodes", "git_attribution_edges"]
            .iter()
            .enumerate()
        {
            if context
                .query(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
                    &[SqlValue::Text((*table).into())],
                )?
                .rows
                .is_empty()
            {
                continue;
            }
            counts[i] = result_u64_from_row(
                &context
                    .query(&format!("SELECT COUNT(*) FROM {table}"), &[])?
                    .rows[0],
                0,
            )?;
        }
        Ok(counts)
    }
}
async fn historical_counts(cell: &RepositoryCell) -> [u64; 2] {
    cell.client
        .query::<InspectHistorical>(&cell.target, None, ())
        .await
        .unwrap()
        .output
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_projection_rebuild_resumes_without_changing_project_data() {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let repository = Uuid::now_v7();
    let started = crate::cells::runtime::test_node(directory.path(), address).await;
    started.client.initialize(repository).await.unwrap();
    let cell = started.client.repository(repository).unwrap();
    let project = project(&cell).await;
    let before = planning(&cell, project).await;
    cell.client
        .command::<Historical>(&cell.target, identity(), 600)
        .await
        .unwrap();
    assert!(
        cell.client
            .query::<GetProjectionState>(&cell.target, None, ProjectionState)
            .await
            .is_err()
    );
    let first = cell
        .client
        .command::<preparation::Prepare>(
            &cell.target,
            identity(),
            PrepareLifecycleInput { version: 0 },
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        first,
        PrepareLifecycleOutcome::Applied(LifecycleStatus::Migrating {
            version: 1,
            copied: 1
        })
    );
    // Stop with a populated old table halfway through its bounded clearing.
    for version in 1..=3 {
        cell.client
            .command::<preparation::Prepare>(
                &cell.target,
                identity(),
                PrepareLifecycleInput { version },
            )
            .await
            .unwrap();
    }
    assert_eq!(historical_counts(&cell).await, [600, 343]);
    started.node.shutdown(ShutdownMode::Preserve).await.unwrap();
    let restored = crate::cells::runtime::test_node(directory.path(), address).await;
    let cell = restored.client.repository(repository).unwrap();
    let mut state = cell
        .client
        .query::<preparation::GetState>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    assert_eq!(
        state,
        LifecycleStatus::Migrating {
            version: 4,
            copied: 3
        }
    );
    assert_eq!(historical_counts(&cell).await, [600, 343]);
    while let LifecycleStatus::Migrating { version, .. } = state {
        let previous: u64 = historical_counts(&cell).await.into_iter().sum();
        let outcome = cell
            .client
            .command::<preparation::Prepare>(
                &cell.target,
                identity(),
                PrepareLifecycleInput { version },
            )
            .await
            .unwrap()
            .output;
        let PrepareLifecycleOutcome::Applied(next) = outcome else {
            panic!("preparation conflicted")
        };
        state = next;
        let remaining: u64 = historical_counts(&cell).await.into_iter().sum();
        assert!(previous - remaining <= 256);
        assert_eq!(planning(&cell, project).await, before);
    }
    assert_eq!(planning(&cell, project).await, before);
    build(&cell, 1).await;
    assert!(
        cell.client
            .query::<GetProjectionState>(&cell.target, None, ProjectionState)
            .await
            .unwrap()
            .output
            .ready_epoch
            .is_some()
    );
    restored
        .node
        .shutdown(ShutdownMode::Preserve)
        .await
        .unwrap();
}
