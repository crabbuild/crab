#![cfg(all(feature = "local", feature = "managed"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crab_sdk::{
    CloneOptions, CommitIdentity, CommitOptions, EntryMode, ErrorKind, FetchDepth, FetchOptions,
    FileEdit, GitPath, LocalCommitOptions, LocalPushOutcome, ManagedOptions,
    ManagedRepositoryState, MutationOutcome, OperationOptions, PullOptions, PushOptions,
    PushRefspec, RepositoryLocator, Revision, WritePolicy,
};

const CONTROL: &str = "CRAB_SDK_MANAGED_FIXTURE_CONTROL";

fn git_path() -> PathBuf {
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = directory.join(if cfg!(windows) { "git.exe" } else { "git" });
        if candidate.is_file() {
            return candidate.canonicalize().unwrap();
        }
    }
    panic!("Git is required for managed SDK qualification")
}

fn run(git: &Path, directory: &Path, args: &[&str]) -> String {
    let output = Command::new(git)
        .current_dir(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct LiveManaged {
    authority: String,
    organization: String,
    token_cache: PathBuf,
    crab: PathBuf,
}

impl LiveManaged {
    fn load() -> Self {
        let token_cache = absolute_env("CRAB_SDK_MANAGED_TOKEN_CACHE");
        let crab = absolute_env("CRAB_SDK_MANAGED_CRAB");
        assert!(token_cache.is_dir());
        assert!(crab.is_file());
        Self {
            authority: std::env::var("CRAB_SDK_MANAGED_AUTHORITY").unwrap(),
            organization: std::env::var("CRAB_SDK_MANAGED_ORGANIZATION").unwrap(),
            token_cache,
            crab,
        }
    }

    fn client(&self, scratch: &Path) -> crab_sdk::Client {
        std::fs::create_dir_all(scratch.join("cache")).unwrap();
        crab_sdk::Client::builder()
            .managed(
                ManagedOptions::new(&self.token_cache)
                    .unwrap()
                    .with_authority(&self.authority)
                    .unwrap(),
            )
            .local_tools(crab_sdk::LocalTools::new(git_path(), &self.crab).unwrap())
            .content_cache(
                crab_sdk::ContentCache::new(&scratch.join("cache"), 256 * 1024 * 1024).unwrap(),
            )
            .build()
            .unwrap()
    }

    fn locator(&self, repository: &str) -> RepositoryLocator {
        RepositoryLocator::managed(&self.authority, &self.organization, repository).unwrap()
    }

    fn fixture(&self, action: &str, scenario: &str, repository: Option<&str>) -> serde_json::Value {
        let control = absolute_env(CONTROL);
        let mut command = Command::new(control);
        command.args([action, scenario]);
        if let Some(repository) = repository {
            command.arg(repository);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "managed fixture {action} {scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn absolute_env(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap());
    assert!(path.is_absolute(), "{name} must be absolute");
    path
}

fn unique_slug(label: &str) -> String {
    format!(
        "sdk-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn identity() -> CommitIdentity {
    CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap()
}

async fn wait_until_active(
    live: &LiveManaged,
    client: &crab_sdk::Client,
    repository: &str,
) -> crab_sdk::RemoteRepository {
    let locator = live.locator(repository);
    let mut last = None;
    for _ in 0..60 {
        match client.open_remote(locator.clone()).await {
            Ok(repository) => return repository,
            Err(error) if matches!(error.kind(), ErrorKind::Conflict | ErrorKind::NotFound) => {
                last = Some(error);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(error) => panic!("managed repository did not become active: {error:?}"),
        }
    }
    panic!("managed repository did not become active: {last:?}")
}

async fn managed_entity(
    management: &crab_sdk::ManagedRepositories,
    organization: &str,
    name: &str,
) -> crab_sdk::ManagedRepository {
    let page = management
        .list(organization, None, 100, OperationOptions::default())
        .await
        .unwrap();
    page.repositories()
        .iter()
        .find(|repository| repository.canonical_url().ends_with(&format!("/{name}")))
        .cloned()
        .unwrap()
}

async fn initial_commit(
    repository: &crab_sdk::RemoteRepository,
    scratch: &Path,
    content: &[u8],
) -> crab_sdk::ObjectId {
    let prepared = repository
        .prepare_commit(
            CommitOptions::initial(
                "refs/heads/main",
                identity(),
                identity(),
                b"managed initial commit\n".to_vec(),
            )
            .unwrap(),
            vec![
                FileEdit::git(
                    GitPath::new("managed.txt").unwrap(),
                    EntryMode::Regular,
                    content.len() as u64,
                    std::io::Cursor::new(content.to_vec()),
                )
                .unwrap(),
            ],
            scratch.to_owned(),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    let commit = prepared.commit_id().unwrap();
    assert!(matches!(
        prepared.execute(OperationOptions::default()).await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    commit
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the real managed-service qualification fixture"]
async fn managed_lifecycle_round_trip() {
    let live = LiveManaged::load();
    let scratch = tempfile::tempdir().unwrap();
    let first_name = unique_slug("lifecycle");
    let second_name = format!("{first_name}-renamed");
    let client = live.client(scratch.path());
    let management = client.managed_repositories().await.unwrap();
    let created = management
        .create(
            &live.organization,
            &first_name,
            &format!("create-{first_name}"),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        created.state(),
        ManagedRepositoryState::Provisioning | ManagedRepositoryState::Active
    ));
    let repository = wait_until_active(&live, &client, &first_name).await;
    let first = initial_commit(&repository, scratch.path(), b"managed first\n").await;
    drop(repository);
    client.close().await.unwrap();

    let client = live.client(scratch.path());
    let checkout = scratch.path().join("checkout");
    let local = client
        .clone_repository(
            live.locator(&first_name),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        run(&git_path(), &checkout, &["rev-parse", "HEAD"]),
        first.to_string()
    );
    std::fs::write(checkout.join("managed.txt"), b"managed second\n").unwrap();
    local.stage(vec!["managed.txt".into()]).await.unwrap();
    let second = local
        .commit(
            LocalCommitOptions::new(identity(), identity(), b"managed local commit\n".to_vec())
                .unwrap(),
        )
        .await
        .unwrap();
    let dry_run = local
        .prepare_push(PushOptions::current_branch().dry_run(true))
        .await
        .unwrap()
        .execute(OperationOptions::default())
        .await
        .unwrap();
    assert!(matches!(dry_run, LocalPushOutcome::DryRun { .. }));
    let pushed = local
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap()
        .execute(OperationOptions::default())
        .await
        .unwrap();
    assert!(matches!(pushed, LocalPushOutcome::Committed { .. }));
    client.close().await.unwrap();

    let client = live.client(scratch.path());
    let repository = wait_until_active(&live, &client, &first_name).await;
    let snapshot = repository
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap();
    assert_eq!(snapshot.commit_id().unwrap(), second);
    assert_eq!(
        snapshot
            .read_blob(GitPath::new("managed.txt").unwrap())
            .await
            .unwrap(),
        b"managed second\n".as_slice()
    );
    let management = client.managed_repositories().await.unwrap();
    let current = managed_entity(&management, &live.organization, &first_name).await;
    let renamed = management
        .rename(
            &live.organization,
            &first_name,
            &second_name,
            current.revision(),
            &format!("rename-{first_name}"),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(
        renamed
            .canonical_url()
            .ends_with(&format!("/{second_name}"))
    );
    client.close().await.unwrap();

    let client = live.client(scratch.path());
    wait_until_active(&live, &client, &second_name).await;
    let management = client.managed_repositories().await.unwrap();
    let current = managed_entity(&management, &live.organization, &second_name).await;
    let archived = management
        .archive(
            &live.organization,
            &second_name,
            current.revision(),
            &format!("archive-{first_name}"),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(archived.state(), ManagedRepositoryState::Archived);
    client.close().await.unwrap();

    let client = live.client(scratch.path());
    let management = client.managed_repositories().await.unwrap();
    let observed = managed_entity(&management, &live.organization, &second_name).await;
    assert_eq!(observed.state(), ManagedRepositoryState::Archived);
    let deleted = live.fixture("delete", &first_name, Some(&second_name));
    let deleted_revision = deleted["revision"].as_u64().unwrap();
    client.close().await.unwrap();

    let client = live.client(scratch.path());
    let management = client.managed_repositories().await.unwrap();
    let restored = management
        .restore(
            &live.organization,
            &second_name,
            deleted_revision,
            &format!("restore-{first_name}"),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        restored.state(),
        ManagedRepositoryState::Provisioning | ManagedRepositoryState::Active
    ));
    wait_until_active(&live, &client, &second_name).await;
    client.close().await.unwrap();
    live.fixture("cleanup", &first_name, Some(&second_name));
}

fn prepared_fixture(live: &LiveManaged, scenario: &str) -> (String, serde_json::Value) {
    let fixture = live.fixture("prepare", scenario, None);
    let repository = fixture["repository"].as_str().unwrap().to_owned();
    (repository, fixture)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires managed-service revocation fault control"]
async fn revoked_push_never_commits() {
    let live = LiveManaged::load();
    let (name, fixture) = prepared_fixture(&live, "revocation");
    let before = fixture["head"].as_str().unwrap().to_owned();
    let scratch = tempfile::tempdir().unwrap();
    let client = live.client(scratch.path());
    let checkout = scratch.path().join("checkout");
    let local = client
        .clone_repository(live.locator(&name), &checkout, CloneOptions::default())
        .await
        .unwrap();
    std::fs::write(checkout.join("revoked.txt"), b"must not commit\n").unwrap();
    local.stage(vec!["revoked.txt".into()]).await.unwrap();
    local
        .commit(LocalCommitOptions::new(identity(), identity(), b"revoked\n".to_vec()).unwrap())
        .await
        .unwrap();
    let prepared = local
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap();
    live.fixture("arm", "reject-next-finalize", Some(&name));
    let error = prepared
        .execute(OperationOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        ErrorKind::Authorization | ErrorKind::Conflict
    ));
    client.close().await.unwrap();
    let client = live.client(scratch.path());
    let snapshot = wait_until_active(&live, &client, &name)
        .await
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap();
    assert_eq!(snapshot.commit_id().unwrap().to_string(), before);
    client.close().await.unwrap();
    live.fixture("cleanup", "revocation", Some(&name));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires managed-service lost-finalize fault control"]
async fn lost_finalize_reuses_push_id() {
    let live = LiveManaged::load();
    let (name, _) = prepared_fixture(&live, "lost-finalize");
    let scratch = tempfile::tempdir().unwrap();
    let client = live.client(scratch.path());
    let checkout = scratch.path().join("checkout");
    let local = client
        .clone_repository(live.locator(&name), &checkout, CloneOptions::default())
        .await
        .unwrap();
    std::fs::write(checkout.join("lost.txt"), b"lost response\n").unwrap();
    local.stage(vec!["lost.txt".into()]).await.unwrap();
    local
        .commit(LocalCommitOptions::new(identity(), identity(), b"lost\n".to_vec()).unwrap())
        .await
        .unwrap();
    let prepared = local
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap();
    let token = prepared.recovery_token().to_json().unwrap();
    live.fixture("arm", "drop-next-finalize-response", Some(&name));
    let outcome = prepared.execute(OperationOptions::default()).await.unwrap();
    assert!(matches!(outcome, LocalPushOutcome::Indeterminate { .. }));
    client.close().await.unwrap();

    let client = live.client(scratch.path());
    let recovery = crab_sdk::LocalPushRecoveryToken::from_json(&token).unwrap();
    let outcome = client
        .reconcile_push(recovery, OperationOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, LocalPushOutcome::Committed { .. }));
    client.close().await.unwrap();
    let report = live.fixture("inspect", "lost-finalize", Some(&name));
    assert_eq!(report["push_ids"].as_array().unwrap().len(), 1);
    live.fixture("cleanup", "lost-finalize", Some(&name));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires managed-service placement migration fault control"]
async fn placement_change_invalidates_cached_access() {
    let live = LiveManaged::load();
    let (name, fixture) = prepared_fixture(&live, "placement-change");
    let scratch = tempfile::tempdir().unwrap();
    let client = live.client(scratch.path());
    let repository = wait_until_active(&live, &client, &name).await;
    let before = repository
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap()
        .read_blob(GitPath::new("managed.txt").unwrap())
        .await
        .unwrap();
    assert_eq!(
        blake3::hash(&before).to_hex().as_str(),
        fixture["before_blake3"].as_str().unwrap()
    );
    client.close().await.unwrap();

    let migrated = live.fixture("arm", "move-placement", Some(&name));
    let client = live.client(scratch.path());
    let after = wait_until_active(&live, &client, &name)
        .await
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap()
        .read_blob(GitPath::new("managed.txt").unwrap())
        .await
        .unwrap();
    assert_eq!(
        blake3::hash(&after).to_hex().as_str(),
        migrated["after_blake3"].as_str().unwrap()
    );
    assert_ne!(before, after);
    client.close().await.unwrap();
    live.fixture("cleanup", "placement-change", Some(&name));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires managed-service expired-grant fault control"]
async fn expired_grant_succeeds_on_a_new_operation() {
    let live = LiveManaged::load();
    let (name, fixture) = prepared_fixture(&live, "expired-grant");
    live.fixture("arm", "expire-next-transfer-grant", Some(&name));
    let scratch = tempfile::tempdir().unwrap();
    let client = live.client(scratch.path());
    let error = client.open_remote(live.locator(&name)).await.err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Authentication);
    let bytes = client
        .open_remote(live.locator(&name))
        .await
        .unwrap()
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap()
        .read_blob(GitPath::new("managed.txt").unwrap())
        .await
        .unwrap();
    assert_eq!(
        blake3::hash(&bytes).to_hex().as_str(),
        fixture["before_blake3"].as_str().unwrap()
    );
    client.close().await.unwrap();
    let report = live.fixture("inspect", "expired-grant", Some(&name));
    assert!(report["transfer_grants"].as_u64().unwrap() >= 2);
    live.fixture("cleanup", "expired-grant", Some(&name));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires managed-service authorization fault control"]
async fn denied_principal_cannot_reuse_another_clients_cache() {
    let live = LiveManaged::load();
    let (name, _) = prepared_fixture(&live, "principal-isolation");
    let scratch = tempfile::tempdir().unwrap();
    let client = live.client(scratch.path());
    wait_until_active(&live, &client, &name)
        .await
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap()
        .read_blob(GitPath::new("managed.txt").unwrap())
        .await
        .unwrap();
    client.close().await.unwrap();

    live.fixture("arm", "deny-current-principal", Some(&name));
    let denied = live.client(scratch.path());
    let error = denied.open_remote(live.locator(&name)).await.err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Authorization);
    denied.close().await.unwrap();
    live.fixture("cleanup", "principal-isolation", Some(&name));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a real managed repository and fixture control"]
async fn managed_local_matrix_contract() {
    let live = LiveManaged::load();
    let (name, _) = prepared_fixture(&live, "local-matrix");
    let scratch = tempfile::tempdir().unwrap();
    let client = live.client(scratch.path());
    let checkout = scratch.path().join("checkout");
    let local = client
        .clone_repository(
            live.locator(&name),
            &checkout,
            CloneOptions::default().with_depth(1).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        run(&git_path(), &checkout, &["rev-list", "--count", "HEAD"]),
        "1"
    );
    assert!(
        local
            .fetch(
                FetchOptions::default()
                    .with_depth(FetchDepth::Deepen(1))
                    .unwrap(),
            )
            .await
            .unwrap()
            .is_shallow()
    );
    assert!(
        !local
            .fetch(
                FetchOptions::default()
                    .with_depth(FetchDepth::Unshallow)
                    .unwrap(),
            )
            .await
            .unwrap()
            .is_shallow()
    );

    let advanced = live.fixture("arm", "advance-main", Some(&name));
    local
        .pull(PullOptions::fast_forward_only().hydrate(false))
        .await
        .unwrap();
    assert_eq!(
        run(&git_path(), &checkout, &["rev-parse", "HEAD"]),
        advanced["head"].as_str().unwrap()
    );

    let linked = scratch.path().join("linked");
    run(
        &git_path(),
        &checkout,
        &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    let sibling = client.open_local(&linked).await.unwrap();
    assert_eq!(sibling.common_directory(), local.common_directory());
    drop(sibling);

    std::fs::write(checkout.join("matrix.txt"), b"managed matrix\n").unwrap();
    local.stage(vec!["matrix.txt".into()]).await.unwrap();
    local
        .commit(LocalCommitOptions::new(identity(), identity(), b"matrix\n".to_vec()).unwrap())
        .await
        .unwrap();
    run(&git_path(), &checkout, &["tag", "sdk-matrix"]);
    let options = PushOptions::current_branch()
        .with_refspecs(vec![
            PushRefspec::update("refs/heads/main", "refs/heads/main").unwrap(),
            PushRefspec::update("refs/tags/sdk-matrix", "refs/tags/sdk-matrix").unwrap(),
        ])
        .unwrap();
    assert!(matches!(
        local
            .prepare_push(options.clone().dry_run(true))
            .await
            .unwrap()
            .execute(OperationOptions::default())
            .await
            .unwrap(),
        LocalPushOutcome::DryRun { .. }
    ));
    let atomic = local
        .prepare_push(options)
        .await
        .unwrap()
        .execute(OperationOptions::default())
        .await;
    match atomic {
        Ok(LocalPushOutcome::Committed { .. }) => {}
        Err(error) if matches!(error.kind(), ErrorKind::UnsupportedCapability) => {}
        other => panic!("managed atomic push returned an invalid outcome: {other:?}"),
    }

    run(&git_path(), &checkout, &["reset", "--hard", "HEAD~1"]);
    std::fs::write(checkout.join("forced.txt"), b"forced\n").unwrap();
    run(&git_path(), &checkout, &["add", "forced.txt"]);
    run(
        &git_path(),
        &checkout,
        &[
            "-c",
            "user.name=SDK qualification",
            "-c",
            "user.email=sdk@example.invalid",
            "commit",
            "-m",
            "forced",
        ],
    );
    let forced = local
        .prepare_push(PushOptions::current_branch().with_policy(WritePolicy::ForceWithLease))
        .await
        .unwrap()
        .execute(OperationOptions::default())
        .await;
    match forced {
        Ok(LocalPushOutcome::Committed { .. }) => {}
        Err(error) if matches!(error.kind(), ErrorKind::Authorization | ErrorKind::Conflict) => {}
        other => panic!("managed force-with-lease returned an invalid outcome: {other:?}"),
    }

    let config = Command::new(git_path())
        .current_dir(&checkout)
        .args([
            "config",
            "--get-regexp",
            "^(remote\\..*\\.promisor|core\\.sparseCheckout)$",
        ])
        .output()
        .unwrap();
    assert_eq!(config.status.code(), Some(1));
    assert!(config.stdout.is_empty());

    let sha256 = scratch.path().join("sha256");
    std::fs::create_dir(&sha256).unwrap();
    run(
        &git_path(),
        &sha256,
        &["init", "--initial-branch=main", "--object-format=sha256"],
    );
    std::fs::write(sha256.join("retained.txt"), b"retained\n").unwrap();
    let error = client.open_local(&sha256).await.err().unwrap();
    assert_eq!(error.kind(), ErrorKind::UnsupportedCapability);
    assert_eq!(
        std::fs::read(sha256.join("retained.txt")).unwrap(),
        b"retained\n"
    );
    client.close().await.unwrap();
    live.fixture("cleanup", "local-matrix", Some(&name));
}
