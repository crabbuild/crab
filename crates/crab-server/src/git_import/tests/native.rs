//! Imports from a stock Git host over protocol-v2 smart HTTP.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use reqwest::{Method, StatusCode};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::wait_for_state;
use crate::test_support::{git::success as git, git_host::GitHost, http::Harness};

pub(super) async fn harness(prefix: &str) -> Harness {
    let h = Harness::new_with_store_and_prefix(
        false,
        false,
        crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new())),
        prefix,
    )
    .await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    h
}

pub(super) async fn commit(path: &Path, file: &str, body: &str, message: &str) {
    std::fs::write(path.join(file), body).unwrap();
    git(path, &["add", file]).await;
    git(path, &["commit", "-m", message]).await;
}

async fn start(h: &Harness, cookie: &str, source: &str) -> String {
    let id = Uuid::now_v7();
    let response = h
        .write(
            Method::POST,
            "/api/imports/git",
            cookie,
            json!({"request_id": id, "source": source, "owner": "team", "name": "imported"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    format!("/api/imports/git/{id}")
}

/// Every branch and tag of `path`, as `git for-each-ref` reports them.
pub(super) async fn branches_and_tags(path: &Path) -> BTreeMap<String, String> {
    git(
        path,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
        ],
    )
    .await
    .lines()
    .map(|line| {
        let (name, oid) = line.split_once(' ').unwrap();
        (name.to_owned(), oid.to_owned())
    })
    .collect()
}

async fn clone_url(h: &Harness, cookie: &str) -> String {
    let token: serde_json::Value = h
        .write(
            Method::POST,
            "/api/git-token",
            cookie,
            json!({"owner":"team","repository":"imported","access":"read"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/imported.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote
        .set_password(Some(token["token"].as_str().unwrap()))
        .unwrap();
    remote.into()
}

#[tokio::test(flavor = "multi_thread")]
async fn imports_exactly_the_sources_branches_and_tags_over_smart_http() {
    let h = harness("native-import").await;
    let cookie = h.login().await;
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=trunk", "."]).await;
    commit(&source, "a.txt", "one\n", "First").await;
    git(&source, &["checkout", "-b", "feature"]).await;
    commit(&source, "b.txt", "feature\n", "Feature work").await;
    git(&source, &["checkout", "trunk"]).await;
    commit(&source, "c.txt", "two\n", "Second").await;
    git(
        &source,
        &["merge", "--no-ff", "feature", "-m", "Merge feature"],
    )
    .await;
    git(&source, &["tag", "-a", "v1", "-m", "Release one"]).await;
    git(&source, &["tag", "light", "HEAD~1"]).await;
    git(&source, &["tag", "tree", "HEAD^{tree}"]).await;
    // Provider pull refs and notes are outside the imported namespaces; their
    // unique history must not be fetched either.
    git(&source, &["checkout", "--orphan", "hidden"]).await;
    commit(&source, "secret.txt", "hidden\n", "Hidden").await;
    let hidden = git(&source, &["rev-parse", "HEAD"]).await;
    git(&source, &["checkout", "trunk"]).await;
    git(&source, &["branch", "-D", "hidden"]).await;
    git(&source, &["update-ref", "refs/pull/1/head", &hidden]).await;
    git(&source, &["notes", "add", "-m", "Reviewed", "HEAD"]).await;
    let expected = branches_and_tags(&source).await;
    assert_eq!(expected.len(), 5);

    let host = GitHost::serve(sources.path()).await;
    let status = start(&h, &cookie, &host.url("source")).await;
    wait_for_state(&h, &cookie, &status, "succeeded").await;
    assert_eq!(host.fetches(), 1);

    let repository = h
        .server
        .repositories
        .get(&("team".to_owned(), "imported".to_owned()))
        .unwrap();
    let view = repository.open_ref_view().await.unwrap();
    assert_eq!(view.head(), "refs/heads/trunk");
    assert_eq!(view.refs(), &expected);
    let catalog = h.server.catalog().unwrap();
    let root = catalog.root();
    let staging = root
        .store
        .list_prefix(&root.path(".crab/http-server/v1/git-imports/staging"))
        .await
        .unwrap();
    assert!(
        staging.is_empty(),
        "published imports remove their staged pack"
    );

    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &[
            "-c",
            "protocol.version=2",
            "clone",
            "--mirror",
            &clone_url(&h, &cookie).await,
            ".",
        ],
    )
    .await;
    assert_eq!(branches_and_tags(clone.path()).await, expected);
    git(clone.path(), &["fsck", "--strict", "--no-dangling"]).await;
    assert!(
        crate::test_support::git::git(clone.path(), &["cat-file", "-e", &hidden])
            .await
            .status
            .code()
            != Some(0),
        "history reachable only from refs/pull must stay behind"
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn import_resumes_from_its_staged_pack_once_lfs_payloads_are_available() {
    let h = harness("native-import-lfs").await;
    let cookie = h.login().await;
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=main", "."]).await;
    let payload = b"large payload stored outside Git".repeat(1024);
    let pointer = crab_git::LfsPointer {
        oid: Sha256::digest(&payload).into(),
        size: payload.len() as u64,
        extensions: Vec::new(),
    };
    std::fs::write(source.join("asset.bin"), pointer.serialize()).unwrap();
    git(&source, &["add", "asset.bin"]).await;
    git(&source, &["commit", "-m", "Add large file"]).await;
    let host = GitHost::serve(sources.path()).await;

    // Refs stay invisible until every pointer has a verified payload.
    let status = start(&h, &cookie, &host.url("source")).await;
    wait_for_state(&h, &cookie, &status, "failed").await;
    assert!(
        !h.json("/api/repos", &cookie).await["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|repository| repository["name"] == "imported"),
        "a failed import publishes no repository"
    );

    host.offer_lfs(&payload);
    let resumed = h
        .write(
            Method::POST,
            &format!("{status}/resume"),
            &cookie,
            json!({}),
        )
        .await;
    assert_eq!(resumed.status(), StatusCode::ACCEPTED);
    wait_for_state(&h, &cookie, &status, "succeeded").await;
    assert_eq!(host.fetches(), 1, "the resumed job reuses its staged pack");

    let repository = h
        .server
        .repositories
        .get(&("team".to_owned(), "imported".to_owned()))
        .unwrap();
    let copied = crab_lfs::LfsObjectStore::new(
        repository.layout.store().clone(),
        repository.layout.repo_prefix(),
    );
    assert!(copied.exists(&pointer.oid).await.unwrap());
    let view = repository.open_ref_view().await.unwrap();
    assert_eq!(view.refs(), &branches_and_tags(&source).await);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "fetches public repositories from github.com"]
async fn mirrors_public_github_repositories_with_exactly_their_branches_and_tags() {
    let h = harness("github-mirror").await;
    let cookie = h.login().await;
    let scratch = tempfile::tempdir().unwrap();
    for (index, source) in ["octocat/Hello-World", "BurntSushi/ripgrep"]
        .into_iter()
        .enumerate()
    {
        let name = format!("live-{index}");
        let url = format!("https://github.com/{source}.git");
        let id = Uuid::now_v7();
        let started = std::time::Instant::now();
        let response = h
            .write(
                Method::POST,
                "/api/imports/git",
                &cookie,
                json!({"request_id": id, "source": source, "owner": "team", "name": name, "mirror": true}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let status = format!("/api/imports/git/{id}");
        let state = tokio::time::timeout(std::time::Duration::from_secs(600), async {
            loop {
                let job = h.json(&status, &cookie).await;
                match job["state"].as_str().unwrap() {
                    "succeeded" | "failed" => return job,
                    _ => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(state["state"], "succeeded", "{source}: {state}");
        eprintln!("{source}: imported in {:?}", started.elapsed());
        let advertised = git(scratch.path(), &["ls-remote", "--heads", "--tags", &url])
            .await
            .lines()
            .filter(|line| !line.ends_with("^{}"))
            .map(|line| {
                let (oid, name) = line.split_once('\t').unwrap();
                (name.to_owned(), oid.to_owned())
            })
            .collect::<BTreeMap<_, _>>();
        let repository = h
            .server
            .repositories
            .get(&("team".to_owned(), name.clone()))
            .unwrap();
        assert_eq!(
            repository.open_ref_view().await.unwrap().refs(),
            &advertised
        );
        let synced: serde_json::Value = h
            .write(
                Method::POST,
                &format!("/api/repositories/{}/mirror/sync", repository.id),
                &cookie,
                json!({}),
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(synced["status"]["updated_refs"], 0, "{source}: {synced}");
        let token: serde_json::Value = h
            .write(
                Method::POST,
                "/api/git-token",
                &cookie,
                json!({"owner":"team","repository":name,"access":"read"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut remote = url::Url::parse(&format!("{}/git/team/{name}.git", h.origin)).unwrap();
        remote.set_username("crab").unwrap();
        remote
            .set_password(Some(token["token"].as_str().unwrap()))
            .unwrap();
        let clone = scratch.path().join(&name);
        std::fs::create_dir(&clone).unwrap();
        git(&clone, &["clone", "--mirror", remote.as_str(), "."]).await;
        assert_eq!(branches_and_tags(&clone).await, advertised);
        git(&clone, &["fsck", "--strict", "--no-dangling"]).await;
    }
    h.close().await;
}
