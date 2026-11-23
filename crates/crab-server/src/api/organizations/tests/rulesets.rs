use super::{repositories::repository_write, *};
use crate::test_support::git;

mod properties;

fn rule(id: Uuid, name: &str, target: &str, refs: Value) -> Value {
    let branch = target == "branch";
    json!({
        "id":id,"name":name,"target":target,"enforcement":"active","include":refs,"exclude":[],
        "restrict_creations":!branch,"restrict_deletions":false,"require_pull_request":branch,
        "required_approvals":if branch { 1 } else { 0 },"required_code_owner_reviews":false,
        "required_checks":[],"required_integrations":[],"bypass_actors":[],
    })
}

async fn create(h: &Harness, org: &str, alice: &str, name: &str) -> (Uuid, String) {
    let repo: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            alice,
            json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        Uuid::parse_str(repo["id"].as_str().unwrap()).unwrap(),
        repo["owner"].as_str().unwrap().into(),
    )
}

async fn remote(h: &Harness, alice: &str, owner: &str, name: &str) -> url::Url {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            alice,
            json!({"owner":owner,"repository":name,"access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/{owner}/{name}.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    remote
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_repository_names_follow_current_targets_and_requester_bypass() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "org-rename-policy").await;
    h.initialize_directory().await;
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let org = format!("/api/organizations/{organization}");
    let rules = format!("{org}/rulesets");
    let (release, owner) = create(&h, &org, &alice, "release-app").await;
    let (sandbox, _) = create(&h, &org, &alice, "release-sandbox").await;
    let release_name = format!("/api/repositories/{release}/name");
    let sandbox_name = format!("/api/repositories/{sandbox}/name");
    let mut owner_rule = rule(Uuid::now_v7(), "Release policy", "branch", json!(["main"]));
    owner_rule["bypass_actors"] = json!([{"actor":{"kind":"organization_owner"},"mode":"always"}]);
    let mut pr_only = rule(Uuid::now_v7(), "Review policy", "branch", json!(["main"]));
    pr_only["bypass_actors"] =
        json!([{"actor":{"kind":"organization_owner"},"mode":"pull_request"}]);
    let targets = json!({
        "kind":"name","include":["release-*"],"exclude":["release-sandbox"],"protected":true
    });
    let rulesets = |pr_only: &Value| {
        json!([
            {"ruleset":owner_rule,"repositories":targets},
            {"ruleset":pr_only,"repositories":targets}
        ])
    };
    h.write(
        Method::PUT,
        &rules,
        &alice,
        json!({"expected_version":0,"rulesets":rulesets(&pr_only)}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&rules, &alice).await["rulesets"][0]["repositories"],
        targets
    );

    let change = |name: &str| json!({"request_id":Uuid::now_v7(),"version":0,"name":name});
    for name in ["unregulated", "release-other", "Release-app"] {
        let rejected = h
            .write(Method::PUT, &release_name, &alice, change(name))
            .await;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            rejected.json::<Value>().await.unwrap()["error"]["code"],
            "repository_name_protected"
        );
    }
    let original = h.json(&release_name, &alice).await;
    assert_eq!(
        original,
        json!({"id":release,"owner":owner,"name":"release-app","version":0})
    );
    h.write(Method::PUT, &release_name, &alice, change("release-app"))
        .await
        .error_for_status()
        .unwrap();
    let enter = change("release-preview");
    for _ in 0..2 {
        let renamed = h
            .write(Method::PUT, &sandbox_name, &alice, enter.clone())
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(
            renamed,
            json!({"id":sandbox,"owner":owner,"name":"release-preview","version":1})
        );
    }

    let offer = h
        .write(
            Method::POST,
            &format!("/api/repositories/{release}/transfer"),
            &alice,
            json!({
                "request_id":Uuid::now_v7(),"version":0,"name":"unregulated",
                "recipient":{"kind":"account","handle":"bob"}
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let transfer = format!(
        "/api/repository-transfers/{}",
        offer["id"].as_str().unwrap()
    );
    let accept = || h.write(Method::PUT, &transfer, &bob, json!({"decision":"accept"}));
    let rejected = accept().await;
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["code"],
        "repository_name_protected"
    );
    assert_eq!(h.json(&release_name, &alice).await, original);
    assert_eq!(h.json(&transfer, &bob).await["state"], "pending");

    pr_only["enforcement"] = json!("disabled");
    h.write(
        Method::PUT,
        &rules,
        &alice,
        json!({"expected_version":1,"rulesets":rulesets(&pr_only)}),
    )
    .await
    .error_for_status()
    .unwrap();
    // The accepting account is not a source owner. The source requester's
    // live Always grant now satisfies every matching active rule.
    drop(accept().await.error_for_status().unwrap());
    let accepted: Value = accept()
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(accepted["to"]["name"], "unregulated");
    assert_eq!(h.json(&release_name, &bob).await["id"], json!(release));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inherited_rules_follow_repository_targets_and_keep_bypass_sources_distinct() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "org-rulesets").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, bob_id) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let org_rules = format!("{org}/rulesets");
    let (release_id, owner) = create(&h, &org, &alice, "release-app").await;
    let (sandbox_id, _) = create(&h, &org, &alice, "release-sandbox").await;
    let release_url = remote(&h, &alice, &owner, "release-app").await;
    let sandbox_url = remote(&h, &alice, &owner, "release-sandbox").await;
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), release_url.as_str()).await;
    git::success(source.path(), &["push", sandbox_url.as_str(), "main"]).await;
    let base = git::success(source.path(), &["rev-parse", "main"]).await;
    let release = format!("/api/repos/{owner}/release-app");
    let sandbox = format!("/api/repos/{owner}/release-sandbox");
    let release_rules = format!("{release}/rulesets");
    let sandbox_rules = format!("{sandbox}/rulesets");

    let id = Uuid::now_v7();
    let tag_id = Uuid::now_v7();
    let mut branch = rule(id, "Release approval", "branch", json!(["~DEFAULT_BRANCH"]));
    branch["require_up_to_date"] = json!(true);
    branch["required_checks"] = json!(["ci/release"]);
    branch["bypass_actors"] =
        json!([{"actor":{"kind":"organization_owner"},"mode":"pull_request"}]);
    let tag = rule(tag_id, "Immutable tags", "tag", json!(["v*"]));
    let entries = json!([
        {"ruleset":branch,"repositories":{"kind":"name","include":["release-*"],"exclude":["release-sandbox"]}},
        {"ruleset":tag,"repositories":{"kind":"selected","ids":[release_id]}},
    ]);
    let foreign = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap()
        .id;
    assert_eq!(h.write(Method::PUT, &org_rules, &alice, json!({
        "expected_version":0,"rulesets":[{"ruleset":tag,"repositories":{"kind":"selected","ids":[foreign]}}]
    })).await.status(), StatusCode::BAD_REQUEST);
    let save = json!({"expected_version":0,"rulesets":entries});
    h.write(Method::PUT, &org_rules, &alice, save.clone())
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.write(Method::PUT, &org_rules, &alice, save)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let inherited = h.json(&release_rules, &alice).await;
    assert_eq!(inherited["rulesets"], json!([]));
    assert_eq!(inherited["inherited"]["organization"], json!(organization));
    assert_eq!(
        inherited["inherited"]["rulesets"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        h.json(&sandbox_rules, &alice).await["inherited"]["rulesets"],
        json!([])
    );
    for (refspec, denial) in [
        ("HEAD:refs/heads/main", "requires a pull request"),
        ("HEAD:refs/tags/v1", "restricts ref creation"),
    ] {
        let result = git::git(source.path(), &["push", release_url.as_str(), refspec]).await;
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(denial));
        git::success(source.path(), &["push", sandbox_url.as_str(), refspec]).await;
    }
    assert_ne!(release_id, sandbox_id);

    // A repository administrator can create a local rule with the same UUID.
    // Its bypass neither changes the organization rule nor satisfies it.
    let members = format!("{release}/members");
    let acl = h.json(&members, &alice).await;
    repository_write(&h, Method::PUT, &members, &alice, release_id, json!({
        "expected_revision":acl["revision"],"groups":[],"members":[{"subject":"bob-id","name":"Bob","access":"admin"}]
    })).await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let mut local = rule(id, "Local approval", "branch", json!(["main"]));
    local["bypass_actors"] = json!([{"actor":{"kind":"repository_admin"},"mode":"always"}]);
    repository_write(
        &h,
        Method::PUT,
        &release_rules,
        &bob,
        release_id,
        json!({"expected_version":0,"rulesets":[local]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(
            Method::PUT,
            &org_rules,
            &bob,
            json!({"expected_version":1,"rulesets":[]})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.http
            .get(format!("{}{org_rules}", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    let pulls = format!("{release}/pulls");
    repository_write(&h, Method::POST, &pulls, &alice, release_id,
        json!({"request_id":Uuid::now_v7(),"title":"Release","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    let pull = format!("{pulls}/1");
    let merge = format!("{pull}/merge");
    let detail = h.json(&pull, &bob).await;
    assert_eq!(detail["merge_requirements"]["head_up_to_date"], true);
    let request = |bypass: Value| {
        json!({
            "request_id":Uuid::now_v7(),"version":detail["version"],"method":"fast_forward",
            "base_oid":base,"head_oid":head,"bypass_rulesets":bypass,
        })
    };
    assert_eq!(detail["merge_requirements"]["bypass_options"], json!([]));
    let blocked = repository_write(
        &h,
        Method::POST,
        &merge,
        &bob,
        release_id,
        request(json!([id])),
    )
    .await;
    assert_eq!(blocked.status(), StatusCode::CONFLICT);
    assert_eq!(
        blocked.json::<Value>().await.unwrap()["error"]["code"],
        "merge_blocked"
    );
    let inherited_id = json!({"organization":organization,"ruleset":id});
    let missing = repository_write(
        &h,
        Method::POST,
        &merge,
        &bob,
        release_id,
        request(json!([inherited_id])),
    )
    .await;
    assert_eq!(
        missing.json::<Value>().await.unwrap()["error"]["code"],
        "ruleset_bypass_unavailable"
    );

    // An organization team is eligible without a separate team grant to
    // this repository. Bob still needs his own current repository access.
    h.write(
        Method::PUT,
        &format!("{org}/invitations/{bob_id}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{org}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let team = Uuid::now_v7();
    let commands = format!("{org}/teams/commands");
    h.write(
        Method::POST,
        &commands,
        &alice,
        json!({"request_id":team,"command":{
            "kind":"create","name":"Release operators","description":"","visibility":"closed"
        }}),
    )
    .await
    .error_for_status()
    .unwrap();
    let membership = |version, role: Value| {
        json!({"request_id":Uuid::now_v7(),"command":{
            "kind":"member","team":team,"member":bob_id,"version":version,"role":role,
        }})
    };
    h.write(
        Method::POST,
        &commands,
        &alice,
        membership(0, json!("member")),
    )
    .await
    .error_for_status()
    .unwrap();
    let mut entries = entries;
    entries[0]["ruleset"]["require_merge_queue"] = json!(true);
    entries[0]["ruleset"]["restrict_updates"] = json!(true);
    entries[0]["ruleset"]["bypass_actors"] = json!([
        {"actor":{"kind":"organization_owner"},"mode":"pull_request"},
        {"actor":{"kind":"team","id":team},"mode":"pull_request"},
    ]);
    h.write(
        Method::PUT,
        &org_rules,
        &alice,
        json!({"expected_version":1,"rulesets":entries}),
    )
    .await
    .error_for_status()
    .unwrap();
    let member = h.json(&pull, &bob).await;
    assert_eq!(member["merge_queue_required"], true);
    assert_eq!(member["merge_requirements"]["updates_restricted"], true);
    let frozen = git::git(
        source.path(),
        &["push", release_url.as_str(), "feature:main"],
    )
    .await;
    assert!(!frozen.status.success());
    assert!(String::from_utf8_lossy(&frozen.stderr).contains("restricts ref updates"));
    assert!(
        member["merge_requirements"]["bypass_options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|option| option["id"] == inherited_id)
    );
    h.write(Method::POST, &commands, &alice, membership(1, Value::Null))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.json(&pull, &bob).await["merge_requirements"]["bypass_options"],
        json!([])
    );

    let detail = h.json(&pull, &alice).await;
    let options = detail["merge_requirements"]["bypass_options"]
        .as_array()
        .unwrap();
    let ids: Vec<_> = options.iter().map(|option| option["id"].clone()).collect();
    assert_eq!(ids, vec![json!(id), inherited_id]);
    let done: Value = repository_write(
        &h,
        Method::POST,
        &merge,
        &alice,
        release_id,
        request(json!(ids)),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(done["merge"]["bypass_rulesets"], json!(ids));
    let advertised = git::success(
        source.path(),
        &["ls-remote", release_url.as_str(), "refs/heads/main"],
    )
    .await;
    assert_eq!(advertised, format!("{head}\trefs/heads/main"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn required_queues_publish_tested_commits_for_local_and_inherited_rulesets() {
    use std::time::Duration;

    use crate::repository::settings::{RulesetScope, with_ruleset_lease};

    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "ruleset-queues").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    for (name, inherited) in [("org-queue", true), ("repo-queue", false)] {
        let (repo, owner) = create(&h, &org, &alice, name).await;
        let root = format!("/api/repos/{owner}/{name}");
        let remote = remote(&h, &alice, &owner, name).await;
        let work = tempfile::tempdir().unwrap();
        let path = work.path();
        let head = git::push_review_branches(path, remote.as_str()).await;
        let base = git::success(path, &["rev-parse", "main"]).await;
        git::success(path, &["push", remote.as_str(), "main:refs/heads/stable"]).await;
        let write = async |method, route: &str, input| {
            repository_write(&h, method, route, &alice, repo, input).await
        };
        write(
            Method::PUT,
            &format!("{root}/settings/merge-methods"),
            json!({
                "expected_version":0,"enabled":["squash"],"auto_merge":true,"merge_queue":false,
            }),
        )
        .await
        .error_for_status()
        .unwrap();

        let mut rule = rule(
            Uuid::now_v7(),
            "Queue default branch",
            "branch",
            json!(["~DEFAULT_BRANCH"]),
        );
        rule["require_pull_request"] = json!(false);
        rule["required_approvals"] = json!(0);
        rule["require_merge_queue"] = json!(true);
        let local = format!("{root}/rulesets");
        assert_eq!(
            write(
                Method::PUT,
                &local,
                json!({"expected_version":0,"rulesets":[rule]})
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        rule["required_checks"] = json!(["ci/test"]);
        if inherited {
            let entry = h.server.repositories.by_id(repo).unwrap();
            let pending_publication: crate::app::Result<_> = with_ruleset_lease(
                &h.server,
                RulesetScope::Organization(organization),
                &h.server.cancellation,
                |_| async {
                    // An old policy read cannot authorize a push while an
                    // organization ruleset update owns the publication gate.
                    let pushed = git::git(path, &["push", remote.as_str(), "feature:main"]).await;
                    assert!(!pushed.status.success());
                    assert_eq!(
                        entry.ref_oid("refs/heads/main").await?.as_deref(),
                        Some(base.as_str())
                    );
                    Ok(())
                },
            )
            .await;
            pending_publication.unwrap();
            h.write(Method::PUT, &format!("{org}/rulesets"), &alice,
                json!({"expected_version":0,"rulesets":[{"ruleset":rule,"repositories":{"kind":"selected","ids":[repo]}}]}))
                .await.error_for_status().unwrap();
        } else {
            write(
                Method::PUT,
                &local,
                json!({"expected_version":0,"rulesets":[rule]}),
            )
            .await
            .error_for_status()
            .unwrap();
        }
        for refspec in ["feature:main", ":main"] {
            let denied = git::git(path, &["push", remote.as_str(), refspec]).await;
            assert!(!denied.status.success());
            assert!(String::from_utf8_lossy(&denied.stderr).contains("requires the merge queue"));
        }
        git::success(path, &["push", remote.as_str(), "feature:stable"]).await;
        assert_eq!(
            git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await,
            format!("{base}\trefs/heads/main")
        );

        let pulls = format!("{root}/pulls");
        write(
            Method::POST,
            &pulls,
            json!({
                "request_id":Uuid::now_v7(),"title":"Checked default branch",
                "base_ref":"refs/heads/main","head_ref":"refs/heads/feature","body":"",
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        let post_status = async |oid: &str| {
            write(
                Method::POST,
                &format!("{root}/statuses/{oid}"),
                json!({
                    "request_id":Uuid::now_v7(),"context":"ci/test","state":"success",
                }),
            )
            .await
            .error_for_status()
            .unwrap();
        };
        post_status(&head).await;
        let pull = format!("{pulls}/1");
        let detail = h.json(&pull, &alice).await;
        assert_eq!(detail["merge_queue_required"], true);
        assert_eq!(detail["can_merge"], false);
        let input = |method: &str| {
            json!({
                "request_id":Uuid::now_v7(),"version":detail["version"],"base_oid":base,"head_oid":head,
                "method":method,
            })
        };
        let direct = write(Method::POST, &format!("{pull}/merge"), input("squash")).await;
        assert_eq!(
            direct.json::<Value>().await.unwrap()["error"]["code"],
            "merge_queue_required"
        );
        let auto = format!("{pull}/auto-merge");
        let scheduled = write(Method::PUT, &auto, input("squash")).await;
        assert_eq!(
            scheduled.json::<Value>().await.unwrap()["error"]["code"],
            "merge_queue_required"
        );
        let mut queue = input("merge_commit");
        queue["queue"] = json!(true);
        assert_eq!(h.json(&auto, &alice).await["can_enqueue"], true);
        write(Method::PUT, &auto, queue)
            .await
            .error_for_status()
            .unwrap();

        let route = format!("{pull}/merge-candidate");
        let tested = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let ready = h.json(&route, &alice).await;
                if ready["candidate"]["current"] == true {
                    break ready["candidate"]["commit"].as_str().unwrap().to_owned();
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .unwrap();
        assert_ne!(tested, head);
        git::success(path, &["fetch", remote.as_str(), "refs/crab/merge-queue/1"]).await;
        assert_eq!(
            git::success(path, &["rev-parse", "FETCH_HEAD"]).await,
            tested
        );
        post_status(&tested).await;
        tokio::time::timeout(Duration::from_secs(30), async {
            while h.json(&pull, &alice).await["state"] != "merged" {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await,
            format!("{tested}\trefs/heads/main")
        );
    }
    worker.abort();
    h.close().await;
}
