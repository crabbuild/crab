use super::*;
use crate::cells::{Address, runtime};
use cellar::sdk::{NodeOutcome, ShutdownMode};

pub(in crate::cells::directory) struct SeedVersionOne;
impl Mutation for SeedVersionOne {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 65_533;
    type Input = Vec<CreateProfileInput>;
    type Output = Vec<ActorProfile>;
    fn execute(
        context: &mut Transaction<'_>,
        inputs: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        for table in [
            "profile_submissions",
            "profiles",
            "accounts",
            "directory_identity",
            "directory_command_receipts",
        ] {
            context.query(&format!("DROP TABLE {table}"), &[])?;
        }
        context.install_schema(include_str!("version_one.sql"))?;
        let mut profiles = Vec::new();
        for (index, input) in inputs.into_iter().enumerate() {
            let profile = ActorProfile {
                id: Uuid::from_u128(index as u128 + 1),
                handle: input.handle.clone(),
                name: input.name.clone(),
                version: 3,
                created_at_ms: 100,
                updated_at_ms: 200,
            };
            let digest = blake3::hash(&serde_json::to_vec(&(
                &input.identity,
                &input.handle,
                &input.name,
            ))?);
            context.query("INSERT INTO actors(id, issuer, subject, handle, name, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, 3, 100, 200)", &[
                SqlValue::Blob(profile.id.as_bytes().to_vec()), SqlValue::Text(input.identity.issuer), SqlValue::Text(input.identity.subject),
                SqlValue::Text(profile.handle.clone()), SqlValue::Text(profile.name.clone()),
            ])?;
            context.query(
                "INSERT INTO actor_submissions(request_id, digest, actor_id) VALUES (?, ?, ?)",
                &[
                    SqlValue::Blob(input.submission_id.as_bytes().to_vec()),
                    SqlValue::Blob(digest.as_bytes().to_vec()),
                    SqlValue::Blob(profile.id.as_bytes().to_vec()),
                ],
            )?;
            profiles.push(profile);
        }
        context.query(
            "UPDATE directory_identity SET app_revision = ? WHERE singleton = 1",
            &[SqlValue::Integer(profiles.len() as i64 * 3)],
        )?;
        Ok(MutationResult::Success(profiles))
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(in crate::cells::directory) struct SourceSnapshot {
    tables: Vec<String>,
    actors: i64,
    progress: bool,
}
pub(in crate::cells::directory) struct InspectSource;
impl Read for InspectSource {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 65_532;
    type Input = ();
    type Output = SourceSnapshot;
    fn execute(context: &mut Transaction<'_>, _: ()) -> Result<Self::Output> {
        let rows = context.query(
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
            &[],
        )?;
        let tables = rows
            .rows
            .iter()
            .map(|row| match row.as_slice() {
                [SqlValue::Text(name)] => Ok(name.clone()),
                _ => Err(Error::Invalid("migration fixture table record is invalid")),
            })
            .collect::<Result<Vec<_>>>()?;
        let rows = context.query("SELECT count(*) FROM actors", &[])?;
        let Some([SqlValue::Integer(actors)]) = rows.rows.first().map(Vec::as_slice) else {
            return Err(Error::Invalid("migration fixture count is invalid"));
        };
        Ok(SourceSnapshot {
            tables,
            actors: *actors,
            progress: context.document::<Progress>(PROGRESS)?.is_some(),
        })
    }
}

async fn stop(node: cellar::sdk::Node) {
    let NodeOutcome::Stopped(report) = node.shutdown(ShutdownMode::Preserve).await.unwrap() else {
        panic!("node did not stop")
    };
    assert!(
        report.cells_drained
            && report.ingress_drained
            && report.application_work_drained
            && !report.drain_timed_out
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_directory_upgrade_preserves_profiles_submissions_and_progress_across_restart() {
    let local = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:8788".parse().unwrap();
    let started = runtime::test_node(local.path(), address).await;
    started.client.initialize(Address::Directory).await.unwrap();
    let inputs: Vec<_> = (0..103)
        .map(|index| CreateProfileInput {
            submission_id: Uuid::from_u128(10_000 + index),
            identity: IdentityKey {
                issuer: "https://identity.test".into(),
                subject: format!("subject-{index}"),
            },
            handle: format!("person-{index}"),
            name: format!("Person {index}"),
        })
        .collect();
    let seeded = started
        .client
        .command::<SeedVersionOne>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            inputs.clone(),
        )
        .await
        .unwrap()
        .output;
    let status = started
        .client
        .query::<GetDirectoryState>(Address::Directory, None, ())
        .await
        .unwrap();
    assert_eq!(
        status.output,
        DirectoryState::Outdated {
            format: 1,
            required_format: 2
        }
    );
    assert!(matches!(
        started
            .client
            .query::<GetOwnProfile>(Address::Directory, None, inputs[0].identity.clone())
            .await,
        Err(Error::SchemaUnavailable)
    ));
    assert!(matches!(
        started.client.initialize(Address::Directory).await,
        Err(Error::SchemaUnavailable)
    ));
    let request = crate::app::mutation_identity().unwrap();
    let first = started
        .client
        .command::<MigrateDirectory>(Address::Directory, request, ())
        .await
        .unwrap();
    assert_eq!(first.output, DirectoryState::Migrating { copied: 100 });
    assert!(matches!(
        started
            .client
            .command::<RegisterAccounts>(
                Address::Directory,
                crate::app::mutation_identity().unwrap(),
                vec![inputs[0].identity.clone()]
            )
            .await,
        Err(Error::SchemaUnavailable)
    ));
    stop(started.node).await;
    let restored = runtime::test_node(local.path(), address).await;
    let replay = restored
        .client
        .command::<MigrateDirectory>(Address::Directory, request, ())
        .await
        .unwrap();
    assert_eq!(replay, first);
    let last = restored
        .client
        .command::<MigrateDirectory>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            (),
        )
        .await
        .unwrap();
    assert!(matches!(
        last.output,
        DirectoryState::Ready { format: 2, .. }
    ));
    for index in [0, 99, 100, 102] {
        let profile = restored
            .client
            .query::<GetOwnProfile>(
                Address::Directory,
                Some(last.receipt),
                inputs[index].identity.clone(),
            )
            .await
            .unwrap()
            .output
            .unwrap();
        assert_eq!(profile, seeded[index]);
        let retried = restored
            .client
            .command::<CreateProfile>(
                Address::Directory,
                crate::app::mutation_identity().unwrap(),
                inputs[index].clone(),
            )
            .await
            .unwrap()
            .output;
        assert_eq!(retried, ProfileOutcome::Saved(profile));
    }
    let registered = restored
        .client
        .command::<RegisterAccounts>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            inputs
                .iter()
                .take(3)
                .map(|input| input.identity.clone())
                .collect(),
        )
        .await
        .unwrap()
        .output;
    assert_eq!(
        registered,
        seeded
            .iter()
            .take(3)
            .map(|profile| profile.id)
            .collect::<Vec<_>>()
    );
    let repeated = restored
        .client
        .command::<MigrateDirectory>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            (),
        )
        .await
        .unwrap();
    assert_eq!(repeated.output, last.output);
    stop(restored.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_legacy_profile_rolls_back_schema_preparation_and_the_whole_copy_batch() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    started.client.initialize(Address::Directory).await.unwrap();
    let inputs = ["valid", "invalid--handle"]
        .into_iter()
        .enumerate()
        .map(|(index, handle)| CreateProfileInput {
            submission_id: Uuid::now_v7(),
            identity: IdentityKey {
                issuer: "https://identity.test".into(),
                subject: index.to_string(),
            },
            handle: handle.into(),
            name: "Person".into(),
        })
        .collect();
    started
        .client
        .command::<SeedVersionOne>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            inputs,
        )
        .await
        .unwrap();
    let before = started
        .client
        .query::<InspectSource>(Address::Directory, None, ())
        .await
        .unwrap();
    assert!(matches!(
        started
            .client
            .command::<MigrateDirectory>(
                Address::Directory,
                crate::app::mutation_identity().unwrap(),
                ()
            )
            .await,
        Err(Error::Invalid(_))
    ));
    let after = started
        .client
        .query::<InspectSource>(Address::Directory, None, ())
        .await
        .unwrap();
    assert_eq!(
        (after.output, after.receipt),
        (before.output, before.receipt)
    );
    stop(started.node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn maximum_width_legacy_identities_fit_the_migration_batch_byte_gate() {
    let local = tempfile::tempdir().unwrap();
    let started = runtime::test_node(local.path(), "127.0.0.1:8788".parse().unwrap()).await;
    started.client.initialize(Address::Directory).await.unwrap();
    let inputs: Vec<_> = (0..100)
        .map(|index| CreateProfileInput {
            submission_id: Uuid::now_v7(),
            identity: IdentityKey {
                issuer: "🦀".repeat(4096),
                subject: format!("{}{:02}", "🦀".repeat(1022), index),
            },
            handle: format!("p{index:038}"),
            name: "🦀".repeat(100),
        })
        .collect();
    let first_identity = inputs[0].identity.clone();
    let profiles = started
        .client
        .command::<SeedVersionOne>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            inputs,
        )
        .await
        .unwrap()
        .output;
    let first = started
        .client
        .command::<MigrateDirectory>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            (),
        )
        .await
        .unwrap();
    assert_eq!(first.output, DirectoryState::Migrating { copied: 100 });
    let last = started
        .client
        .command::<MigrateDirectory>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            (),
        )
        .await
        .unwrap();
    assert!(matches!(
        last.output,
        DirectoryState::Ready { format: 2, .. }
    ));
    let profile = started
        .client
        .query::<GetOwnProfile>(Address::Directory, None, first_identity)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(profile, profiles[0]);
    stop(started.node).await;
}
