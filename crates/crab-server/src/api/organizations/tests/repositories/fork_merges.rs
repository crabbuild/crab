use std::path::Path;

use crab_remote_git::{OperationKind, RepositoryOptions};
use gix_hash::ObjectId;
use sha2::{Digest, Sha256};

use super::{Harness, Method, StatusCode, Uuid, Value, json, repository_write};
use crate::test_support::git::success as git;

pub(super) async fn exercise(
    h: &Harness,
    alice: &str,
    fork_author: &str,
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
    let source = h.server.repositories.by_id(fork).unwrap();
    let suggestions = format!(
        "/api/repos/{}/{}/pulls/recent-pushes",
        source.config.owner, source.config.name
    );
    assert_eq!(h.json(&suggestions, alice).await["items"], json!([]));
    let recent = h.json(&suggestions, fork_author).await;
    assert_eq!(recent["base_ref"], "refs/heads/main");
    assert_eq!(
        recent["items"][0],
        json!({"name":"refs/heads/proposal","oid":head})
    );
    // Use the exact upstream base already fetched by stock Git and the fork's
    // proposal to obtain an independent expected tree for all three methods.
    git(fork_work, &["fetch", upstream_remote, "main"]).await;
    let expected_ancestor = git(fork_work, &["merge-base", "FETCH_HEAD", &head]).await;
    let expected_tree = git(
        fork_work,
        &["merge-tree", "--write-tree", "FETCH_HEAD", &head],
    )
    .await;

    for (method, target) in targets {
        let source = h.server.repositories.by_id(fork).unwrap();
        if method == "merge_commit" {
            let settings = format!(
                "/api/repos/{}/{}/settings/merge-methods",
                source.config.owner, source.config.name
            );
            let existing = h.json(&settings, alice).await;
            repository_write(h, Method::PUT, &settings, alice, fork, json!({
                "expected_version":existing["version"],"enabled":existing["enabled"],"delete_head_branches":true,
            })).await.error_for_status().unwrap();
        }
        let input = json!({"request_id":Uuid::now_v7(),"title":"Merge from a fork","body":"", "base_ref":format!("refs/heads/{target}"),"head_ref":"refs/heads/proposal","head_repository":fork,"expected":{"base_oid":base,"head_oid":head}});
        if method == "squash" {
            let source = h.server.repositories.by_id(fork).unwrap();
            let locked =
                crab_remote::publication::with_leases::<_, crab_remote::publication::Error, _, _>(
                    &source.store,
                    &source.layout,
                    ["refs/heads/proposal".into()],
                    std::time::Duration::from_secs(30),
                    &h.server.cancellation,
                    |_, _| async {
                        let response =
                            repository_write(h, Method::POST, &pulls, alice, id, input.clone())
                                .await;
                        Ok(response.status())
                    },
                )
                .await
                .unwrap();
            assert_eq!(locked, StatusCode::TOO_MANY_REQUESTS);
        }
        let mut opened: Value = repository_write(h, Method::POST, &pulls, alice, id, input.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let number = opened["number"].as_u64().unwrap();
        if method == "squash" {
            let comparison = format!(
                "{pulls}/open-comparison?base_ref=refs%2Fheads%2Fsquash-target&head_ref=refs%2Fheads%2Fproposal"
            );
            assert!(h.json(&comparison, alice).await["pull_number"].is_null());
            assert_eq!(
                h.json(&format!("{comparison}&head_repository={fork}"), alice)
                    .await["pull_number"],
                number
            );
            let mut duplicate = input;
            duplicate["request_id"] = json!(Uuid::now_v7());
            let duplicate = repository_write(h, Method::POST, &pulls, alice, id, duplicate).await;
            assert_eq!(duplicate.status(), StatusCode::CONFLICT);
            assert_eq!(
                duplicate.json::<Value>().await.unwrap()["error"]["pull_number"],
                number
            );
            let recent = h.json(&suggestions, fork_author).await;
            assert!(
                recent["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["name"] != "refs/heads/proposal")
            );
            let detail = format!("{pulls}/{number}");
            let closed: Value = repository_write(
                h,
                Method::PATCH,
                &detail,
                alice,
                id,
                json!({"version":opened["version"],"state":"closed"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
            let reopen = json!({"version":closed["version"],"state":"open"});
            let source = h.server.repositories.by_id(fork).unwrap();
            let locked =
                crab_remote::publication::with_leases::<_, crab_remote::publication::Error, _, _>(
                    &source.store,
                    &source.layout,
                    ["refs/heads/proposal".into()],
                    std::time::Duration::from_secs(30),
                    &h.server.cancellation,
                    |_, _| async {
                        Ok(
                            repository_write(h, Method::PATCH, &detail, alice, id, reopen.clone())
                                .await
                                .status(),
                        )
                    },
                )
                .await
                .unwrap();
            assert_eq!(locked, StatusCode::TOO_MANY_REQUESTS);
            opened = repository_write(h, Method::PATCH, &detail, alice, id, reopen)
                .await
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(opened["state"], "open");
        }
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
                git(
                    upstream_work,
                    &["push", upstream_remote, "main:refs/heads/proposal"],
                )
                .await;
                let source = h.server.repositories.by_id(fork).unwrap();
                let branch_path = format!(
                    "/api/repos/{}/{}/pulls/{number}/head-branch?upstream={id}",
                    source.config.owner, source.config.name
                );
                let mut deletion = h.json(&branch_path, alice).await["branch"].clone();
                assert_eq!(deletion["action"], "delete");
                assert_eq!(deletion["repository_id"], json!(fork));
                deletion["request_id"] = json!(Uuid::now_v7());
                let dependent: Value = repository_write(h, Method::POST, &pulls, alice, id,
                    json!({"request_id":Uuid::now_v7(),"title":"Review the same fork branch","body":"",
                        "base_ref":"refs/heads/rebase-target","head_ref":"refs/heads/proposal",
                        "head_repository":fork,"expected":{"base_oid":base,"head_oid":head}}))
                    .await.error_for_status().unwrap().json().await.unwrap();
                let blocked =
                    repository_write(h, Method::POST, &branch_path, alice, fork, deletion.clone())
                        .await;
                assert_eq!(blocked.status(), StatusCode::CONFLICT);
                assert_eq!(
                    blocked.json::<Value>().await.unwrap()["error"]["code"],
                    "pull_branch_in_use"
                );
                let dependent_path = format!("{pulls}/{}", dependent["number"]);
                repository_write(
                    h,
                    Method::PATCH,
                    &dependent_path,
                    alice,
                    id,
                    json!({"version":dependent["version"],"state":"closed"}),
                )
                .await
                .error_for_status()
                .unwrap();
                repository_write(h, Method::POST, &branch_path, alice, fork, deletion)
                    .await
                    .error_for_status()
                    .unwrap();
                assert!(
                    git(fork_work, &["ls-remote", "origin", "refs/heads/proposal"])
                        .await
                        .is_empty()
                );
                assert_eq!(
                    git(
                        upstream_work,
                        &["ls-remote", upstream_remote, "refs/heads/proposal"]
                    )
                    .await,
                    format!("{base}\trefs/heads/proposal")
                );
                let saved = format!("{pulls}/{number}/merged-comparison");
                let comparison = h.json(&saved, alice).await;
                assert_eq!(comparison["base"], expected_ancestor);
                assert_eq!(comparison["base_tip"], base);
                assert_eq!(comparison["commit"], head);
                let path = crate::api::encode_hex(b"proposal/final");
                let file = h
                    .json(&format!("{saved}/file?path_hex={path}"), alice)
                    .await;
                assert_eq!(file["new"]["text"], "second fork change");
                let history = format!("{saved}/commits");
                let commits = h.json(&history, alice).await;
                assert_eq!(commits["items"].as_array().unwrap().len(), 3);
                assert_eq!(commits["items"][0]["oid"], head);
                let file = h
                    .json(&format!("{history}/{head}/file?path_hex={path}"), alice)
                    .await;
                assert!(file["old"].is_null());
                assert_eq!(file["new"]["text"], "second fork change");
                let mut restoration = h.json(&branch_path, alice).await["branch"].clone();
                assert_eq!(restoration["action"], "restore");
                restoration["request_id"] = json!(Uuid::now_v7());
                repository_write(h, Method::POST, &branch_path, alice, fork, restoration)
                    .await
                    .error_for_status()
                    .unwrap();
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
            _ => {
                assert_eq!(
                    git(
                        upstream_work,
                        &["rev-list", "--parents", "-1", "FETCH_HEAD"]
                    )
                    .await,
                    format!("{commit} {base} {head}")
                );
                let branch_path = format!(
                    "/api/repos/{}/{}/pulls/{number}/head-branch?upstream={id}",
                    source.config.owner, source.config.name
                );
                assert_eq!(h.json(&branch_path, alice).await["cleanup_pending"], true);
                let worker = tokio::spawn(crate::api::auto_merge::run(h.server.clone()));
                tokio::time::timeout(std::time::Duration::from_secs(20), async {
                    while h.json(&branch_path, alice).await["branch"]["action"] != "restore" {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    }
                })
                .await
                .unwrap();
                worker.abort();
                assert!(
                    git(fork_work, &["ls-remote", "origin", "refs/heads/proposal"])
                        .await
                        .is_empty()
                );
                assert_eq!(
                    git(
                        upstream_work,
                        &["ls-remote", upstream_remote, "refs/heads/proposal"]
                    )
                    .await,
                    format!("{base}\trefs/heads/proposal")
                );
                let mut restoration = h.json(&branch_path, alice).await["branch"].clone();
                assert_eq!(restoration["action"], "restore");
                restoration["request_id"] = json!(Uuid::now_v7());
                repository_write(h, Method::POST, &branch_path, alice, fork, restoration)
                    .await
                    .error_for_status()
                    .unwrap();
            }
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

    let layer = git(upstream_work, &["rev-parse", "feature"]).await;
    git(
        upstream_work,
        &[
            "push",
            "--atomic",
            upstream_remote,
            &format!("{layer}:refs/heads/stack-base"),
            &format!("{base}:refs/heads/stack-target"),
        ],
    )
    .await;
    let open = async |body| {
        let response = repository_write(h, Method::POST, &pulls, alice, id, body).await;
        let status = response.status();
        let pull = response.json::<Value>().await.unwrap();
        assert_eq!(status, StatusCode::CREATED, "{pull}");
        pull
    };
    let parent = open(json!({
        "request_id":Uuid::now_v7(),"title":"Upstream layer","body":"",
        "base_ref":"refs/heads/stack-target","head_ref":"refs/heads/stack-base",
        "expected":{"base_oid":base,"head_oid":layer},
    }))
    .await;
    let child = open(json!({
        "request_id":Uuid::now_v7(),"title":"Fork layer","body":"",
        "base_ref":"refs/heads/stack-base","head_ref":"refs/heads/proposal","head_repository":fork,
        "expected":{"base_oid":layer,"head_oid":head},
    }))
    .await;
    let parent_path = format!("{pulls}/{}", parent["number"]);
    repository_write(
        h,
        Method::POST,
        &format!("{parent_path}/merge"),
        alice,
        id,
        json!({
            "request_id":Uuid::now_v7(),"version":parent["version"],"method":"merge_commit",
            "base_oid":base,"head_oid":layer,
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let branch_path = format!("{parent_path}/head-branch");
    let preview = h.json(&branch_path, alice).await["branch"].clone();
    assert_eq!(preview["retarget"]["pulls"], json!([child["number"]]));
    let input = json!({"request_id":Uuid::now_v7(),"action":"delete","version":preview["version"],
        "head_oid":layer,"repository_id":id,"retarget_token":preview["retarget"]["token"]});
    let fork_repo = h.server.repositories.by_id(fork).unwrap();
    let blocked =
        crab_remote::publication::with_leases::<_, crab_remote::publication::Error, _, _>(
            &fork_repo.store,
            &fork_repo.layout,
            ["refs/heads/proposal".into()],
            std::time::Duration::from_secs(30),
            &h.server.cancellation,
            |_, _| async {
                Ok(
                    repository_write(h, Method::POST, &branch_path, alice, id, input.clone())
                        .await
                        .status(),
                )
            },
        )
        .await
        .unwrap();
    assert_eq!(blocked, StatusCode::TOO_MANY_REQUESTS);
    repository_write(h, Method::POST, &branch_path, alice, id, input)
        .await
        .error_for_status()
        .unwrap();
    let retargeted = h.json(&format!("{pulls}/{}", child["number"]), alice).await;
    assert_eq!(retargeted["base_ref"], "refs/heads/stack-target");
    assert_eq!(retargeted["head_oid"], head);
    assert_eq!(repo.ref_oid("refs/heads/stack-base").await.unwrap(), None);
}
