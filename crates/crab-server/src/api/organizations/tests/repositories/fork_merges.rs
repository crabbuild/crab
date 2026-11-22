use std::path::Path;

use crab_remote_git::{OperationKind, RepositoryOptions};
use gix_hash::ObjectId;
use sha2::{Digest, Sha256};

use super::{Harness, Method, StatusCode, Uuid, Value, json, repository_write};
use crate::test_support::git::success as git;

pub(super) async fn exercise(
    h: &Harness,
    alice: &str,
    id: Uuid,
    fork: Uuid,
    (upstream_work, upstream_remote): (&Path, &str),
    fork_work: &Path,
) {
    let repo = h.server.repositories.by_id(id).unwrap();
    let root = format!("/api/repos/{}/{}", repo.config.owner, repo.config.name);
    let pulls = format!("{root}/pulls");
    let base = git(upstream_work, &["rev-parse", "main"]).await;
    let targets = [
        ("squash", "squash-target"),
        ("rebase", "rebase-target"),
        ("merge_commit", "merge-target"),
    ];
    let branches = targets.map(|(_, branch)| format!("{base}:refs/heads/{branch}"));
    git(
        upstream_work,
        &[
            "push",
            "--atomic",
            upstream_remote,
            &branches[0],
            &branches[1],
            &branches[2],
        ],
    )
    .await;

    git(fork_work, &["checkout", "-b", "proposal", "origin/feature"]).await;
    std::fs::create_dir_all(fork_work.join("proposal")).unwrap();
    let payload = b"LFS object stored only in the fork";
    let digest = crate::api::encode_hex(&Sha256::digest(payload));
    let fork_remote = git(fork_work, &["remote", "get-url", "origin"]).await;
    let object_path = format!("info/lfs/objects/{digest}?size={}", payload.len());
    let endpoint = |remote: &str| {
        let mut url = url::Url::parse(remote).unwrap();
        let token = url.password().unwrap().to_owned();
        url.set_password(None).unwrap();
        url.set_username("").unwrap();
        (format!("{url}/{object_path}"), token)
    };
    let (url, token) = endpoint(&fork_remote);
    h.http
        .put(url)
        .basic_auth("crab", Some(token))
        .body(payload.as_slice())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{digest}\nsize {}\n",
        payload.len()
    );
    std::fs::write(fork_work.join("proposal/asset.bin"), pointer).unwrap();
    git(fork_work, &["add", "proposal/asset.bin"]).await;
    for (path, message) in [
        ("proposal/first", "first fork change"),
        ("proposal/final", "second fork change"),
    ] {
        std::fs::write(fork_work.join(path), message).unwrap();
        git(fork_work, &["add", path]).await;
        git(fork_work, &["commit", "-m", message]).await;
    }
    git(fork_work, &["push", "origin", "proposal"]).await;
    let head = git(fork_work, &["rev-parse", "proposal"]).await;
    // Use the exact upstream base already fetched by stock Git and the fork's
    // proposal to obtain an independent expected tree for all three methods.
    git(fork_work, &["fetch", upstream_remote, "main"]).await;
    let expected_tree = git(
        fork_work,
        &["merge-tree", "--write-tree", "FETCH_HEAD", &head],
    )
    .await;

    for (method, target) in targets {
        let opened: Value = repository_write(
            h, Method::POST, &pulls, alice, id,
            json!({"request_id":Uuid::now_v7(),"title":"Merge from a fork","body":"", "base_ref":format!("refs/heads/{target}"),"head_ref":"refs/heads/proposal","head_repository":fork,"expected":{"base_oid":base,"head_oid":head}}),
        ).await.error_for_status().unwrap().json().await.unwrap();
        let number = opened["number"].as_u64().unwrap();
        let path = format!("{pulls}/{number}/merge");
        let request = json!({"request_id":Uuid::now_v7(),"version":opened["version"],"method":method,"base_oid":base,"head_oid":head});
        let merged = repository_write(h, Method::POST, &path, alice, id, request.clone()).await;
        let status = merged.status();
        let merged = merged.json::<Value>().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{method}: {merged}");
        assert_eq!(merged["merge"]["method"], method);
        let commit = merged["merge"]["commit_oid"].as_str().unwrap();
        let replay: Value = repository_write(h, Method::POST, &path, alice, id, request)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(replay["merge"]["commit_oid"], commit);
        git(upstream_work, &["fetch", upstream_remote, target]).await;
        assert_eq!(
            git(upstream_work, &["rev-parse", "FETCH_HEAD"]).await,
            commit
        );
        assert_eq!(
            git(upstream_work, &["rev-parse", "FETCH_HEAD^{tree}"]).await,
            expected_tree
        );
        match method {
            "squash" => {
                assert_eq!(
                    git(upstream_work, &["rev-parse", "FETCH_HEAD^"]).await,
                    base
                );
                let (url, token) = endpoint(upstream_remote);
                let copied = h
                    .http
                    .get(url)
                    .basic_auth("crab", Some(token))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap();
                assert_eq!(copied.as_ref(), payload);
            }
            "rebase" => {
                let range = format!("{base}..FETCH_HEAD");
                let subjects =
                    git(upstream_work, &["log", "--reverse", "--format=%s", &range]).await;
                assert_eq!(subjects, "feature\nfirst fork change\nsecond fork change");
                assert_eq!(
                    git(upstream_work, &["rev-parse", "FETCH_HEAD~3"]).await,
                    base
                );
            }
            _ => assert_eq!(
                git(
                    upstream_work,
                    &["rev-list", "--parents", "-1", "FETCH_HEAD"]
                )
                .await,
                format!("{commit} {base} {head}")
            ),
        }
        if method != "merge_commit" {
            let shutdown = &h.server.cancellation;
            let captured = repo
                .open_current(&h.server, RepositoryOptions::default(), shutdown)
                .await
                .unwrap();
            let op = captured
                .operation(OperationKind::Repository, shutdown)
                .await
                .unwrap();
            let object = op
                .read_object(ObjectId::from_hex(head.as_bytes()).unwrap())
                .await;
            assert!(matches!(
                op.finish(object).await,
                Err(crab_remote_git::Error::ObjectNotFound { .. })
            ));
        }
    }
    assert_eq!(
        git(fork_work, &["ls-remote", "origin", "refs/heads/proposal"]).await,
        format!("{head}\trefs/heads/proposal")
    );
}
