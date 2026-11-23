use super::*;
use crate::{
    catalog::{BrowserNamespace, MembershipActor, RepositoryApplicationState},
    cells::organization::repositories::{AuthorizeCreation, Authorized, Creation},
    test_support::git,
};

mod fork_merges;
mod sync;

pub(super) async fn repository_write(
    h: &Harness,
    method: Method,
    path: &str,
    cookie: &str,
    repository: Uuid,
    body: Value,
) -> reqwest::Response {
    let session = h.json("/api/session", cookie).await;
    h.http
        .request(method, format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repository.to_string())
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn private_fork_recovers_git_publication_and_uses_live_source_permissions() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "private-fork").await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, owner_actor, _) =
        crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let created: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let owner = created["owner"].as_str().unwrap();
    let source = format!("/api/repos/{owner}/library");
    let fork = format!("/api/repos/{owner}/library-fork");
    let acl = h.json(&format!("{source}/members"), &alice).await;
    repository_write(&h, Method::PUT, &format!("{source}/members"), &alice, id,
        json!({"expected_revision":acl["revision"],"members":[{"subject":"bob-id","name":"Bob","access":"write"}],"groups":[]}))
        .await.error_for_status().unwrap();
    let git_url =
        |name: &str| url::Url::parse(&format!("{}/git/{owner}/{name}.git", h.origin)).unwrap();
    let token = |name: &'static str, cookie: &str| {
        let body = json!({"owner":owner,"repository":name,"access":"write"});
        let h = &h;
        let cookie = cookie.to_owned();
        async move {
            h.write(Method::POST, "/api/git-token", &cookie, body)
                .await
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let source_token = token("library", &alice).await;
    let mut remote = git_url("library");
    remote.set_username("crab").unwrap();
    remote.set_password(source_token["token"].as_str()).unwrap();
    let local = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(local.path(), remote.as_str()).await;
    let request = Uuid::now_v7();
    let body = json!({"request_id":request,"name":"library-fork"});
    let route = format!("{source}/forks");
    assert_eq!(
        repository_write(&h, Method::POST, &route, &bob, id, body.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    h.drop_next_import_reply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        repository_write(&h, Method::POST, &route, &alice, id, body.clone())
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    let status_url = format!("{org}/repositories/imports/git/{request}");
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            let status = h.json(&status_url, &alice).await;
            match status["state"].as_str() {
                Some("succeeded") => break,
                Some("queued" | "running") => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await
                }
                _ => panic!("Fork did not complete: {status}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(
        !h.drop_next_import_reply
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    let replay: Value = repository_write(&h, Method::POST, &route, &alice, id, body)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["state"], "succeeded");
    let grants = h
        .json(
            &format!("{org}/members/{owner_actor}/repository-access"),
            &alice,
        )
        .await;
    let fork_grant = grants["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|repo| repo["name"] == "library-fork")
        .unwrap();
    assert_eq!(fork_grant["inherited_from"], "library");
    assert_eq!(fork_grant["access"], "admin");

    let listed = format!("/api/repos?organization={organization}");
    let visible = h.json(&listed, &bob).await;
    let repo = visible["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|repo| repo["name"] == "library-fork")
        .unwrap();
    assert_eq!(
        repo["forked_from"],
        json!({"id":id,"owner":owner,"name":"library"})
    );
    let fork_id = Uuid::parse_str(repo["id"].as_str().unwrap()).unwrap();
    for (filter, names, fork_marker) in [
        (
            "fork:only%20archived:false",
            vec!["library-fork"],
            Some(true),
        ),
        ("fork:false", vec!["library"], Some(false)),
        ("fork:true", vec!["library", "library-fork"], None),
    ] {
        let result = h
            .json(&format!("{listed}&q=library%20{filter}"), &bob)
            .await;
        let rows = result["repositories"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|repo| repo["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            names
        );
        if let Some(marker) = fork_marker {
            assert_eq!(rows[0]["fork"], marker);
        }
    }
    let fork_token = token("library-fork", &bob).await;
    let secret = fork_token["token"].as_str().unwrap();
    let mut remote = git_url("library-fork");
    remote.set_username("crab").unwrap();
    remote.set_password(Some(secret)).unwrap();
    let clone = tempfile::tempdir().unwrap();
    git::success(clone.path(), &["clone", remote.as_str(), "."]).await;
    assert_eq!(
        git::success(clone.path(), &["rev-parse", "origin/feature"]).await,
        head
    );
    git::success(
        local.path(),
        &["push", remote.as_str(), "HEAD:refs/heads/fork-only"],
    )
    .await;
    let source_refs = h.json(&format!("{source}/refs"), &alice).await;
    assert!(
        !source_refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "refs/heads/fork-only")
    );
    let mut upstream_remote = git_url("library");
    upstream_remote.set_username("crab").unwrap();
    upstream_remote
        .set_password(source_token["token"].as_str())
        .unwrap();
    sync::exercise(
        &h,
        &alice,
        &bob,
        &fork,
        fork_id,
        upstream_remote.as_str(),
        local.path(),
        clone.path(),
    )
    .await;
    fork_merges::exercise(
        &h,
        &alice,
        id,
        fork_id,
        (local.path(), upstream_remote.as_str()),
        clone.path(),
    )
    .await;
    let rule_id = Uuid::now_v7();
    let rulesets = format!("{org}/rulesets");
    let mut entry = json!({
        "ruleset": {"id":rule_id,"name":"Fork branch policy","target":"branch","enforcement":"active",
            "include":["policy/*"],"exclude":[],"restrict_creations":true,"restrict_deletions":false,
            "require_pull_request":false,"required_approvals":0,"required_code_owner_reviews":false,
            "required_checks":[],"required_integrations":[]},
        "repositories":{"kind":"properties","conditions":[
            {"system":"fork","value":true},{"system":"name","value":"*-fork"}
        ]}
    });
    let save = async |version, entry| {
        h.write(
            Method::PUT,
            &rulesets,
            &alice,
            json!({"expected_version":version,"rulesets":[entry]}),
        )
        .await
    };
    let mut invalid = entry.clone();
    invalid["repositories"]["conditions"][0]["value"] = json!("true");
    assert_eq!(save(0, invalid).await.status(), StatusCode::BAD_REQUEST);
    save(0, entry.clone()).await.error_for_status().unwrap();
    assert_eq!(
        h.json(&format!("{fork}/rulesets"), &bob).await["inherited"]["rulesets"][0]["id"],
        json!(rule_id)
    );
    assert_eq!(
        h.json(&format!("{source}/rulesets"), &bob).await["inherited"]["rulesets"],
        json!([])
    );
    let restricted = "HEAD:refs/heads/policy/candidate";
    let blocked = git::git(local.path(), &["push", remote.as_str(), restricted]).await;
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("restricts ref creation"));
    git::success(
        local.path(),
        &["push", upstream_remote.as_str(), restricted],
    )
    .await;
    entry["repositories"]["conditions"][1]["value"] = json!("other-*");
    save(1, entry).await.error_for_status().unwrap();
    git::success(local.path(), &["push", remote.as_str(), restricted]).await;
    let compare = format!(
        "{fork}/changes?rev=refs/heads/fork-only&base=refs/heads/main&merge_base=true&upstream={id}"
    );
    let comparison = h.json(&compare, &bob).await;
    assert_eq!(comparison["commit"], head);
    let base = comparison["base"].as_str().unwrap();
    let diff = h
        .json(
            &format!("{fork}/diff?rev={head}&base={base}&path=README.md"),
            &bob,
        )
        .await;
    assert_eq!(diff["old"]["text"], "base\n");
    assert_eq!(diff["new"]["text"], "base\nfeature\n");
    let payload = h
        .http
        .get(format!(
            "{}/info/lfs/objects/{}?size=5",
            git_url("library-fork"),
            sync::PAYLOAD_OID
        ))
        .basic_auth("crab", Some(secret))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(payload.as_ref(), b"hello");
    let acl = h.json(&format!("{source}/members"), &alice).await;
    let edit = json!({"expected_revision":acl["revision"],"members":[],"groups":[]});
    assert_eq!(
        repository_write(
            &h,
            Method::PUT,
            &format!("{fork}/members"),
            &alice,
            fork_id,
            edit.clone()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    repository_write(
        &h,
        Method::PUT,
        &format!("{source}/members"),
        &alice,
        id,
        edit,
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&listed, &bob).await["repositories"], json!([]));
    assert_eq!(
        h.json(&format!("{listed}&q=fork:only"), &bob).await["repositories"],
        json!([])
    );
    let compare_denied = h
        .http
        .get(format!("{}{compare}", h.origin))
        .header(header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(compare_denied.status(), StatusCode::NOT_FOUND);
    let denied = h
        .http
        .get(format!(
            "{}/info/refs?service=git-upload-pack",
            git_url("library-fork")
        ))
        .basic_auth("crab", Some(secret))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

#[tokio::test]
async fn organization_repository_ownership_survives_creator_revocation_without_an_acl_grant() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h =
        Harness::new_with_store_and_prefix(false, false, store, "organization-onboarding").await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (id, alice_id, bob_id) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let root = format!("/api/organizations/{id}");
    let creation = format!("{root}/repositories/creation");
    let listed = format!("/api/repos?organization={id}");
    let request_id = Uuid::now_v7();
    let body = json!({"request_id":request_id,"name":"widget","description":"Organization owned"});
    h.write(
        Method::PUT,
        &format!("{root}/invitations/{bob_id}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{root}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(Method::POST, &creation, &bob, body.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.http
            .post(format!("{}{creation}", h.origin))
            .header(header::COOKIE, &alice)
            .header(header::ORIGIN, &h.origin)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    let qualified = MembershipActor {
        issuer: h.provider.issuer.clone(),
        subject: "alice-id".into(),
    };
    let ticket = h
        .server
        .cells()
        .unwrap()
        .command::<AuthorizeCreation>(
            Address::Organization(id),
            app::mutation_identity().unwrap(),
            Creation {
                actor: alice_id,
                request_id,
                name: "widget".into(),
                description: "Organization owned".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(ticket.output, Authorized::Accepted));
    let pending = catalog
        .reserve_browser_repository(
            BrowserNamespace::Organization(id),
            "widget".into(),
            "Organization owned".into(),
            request_id,
            qualified,
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(
        pending.application,
        RepositoryApplicationState::EmptyCellPending
    );
    let owner = h.json(&creation, &alice).await["owner"]
        .as_str()
        .unwrap()
        .to_owned();
    let repo = format!("/api/repos/{owner}/widget");
    let members = format!("{repo}/members");
    let expected = json!({"id":pending.id,"owner":owner,"name":"widget"});
    for _ in 0..2 {
        let response: Value = h
            .write(Method::POST, &creation, &alice, body.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response, expected);
    }
    assert_eq!(h.json(&listed, &bob).await["repositories"], json!([]));
    let owners = h.json(&listed, &alice).await;
    assert_eq!(owners["repositories"][0]["organization_id"], json!(id));
    assert_eq!(owners["repositories"][0]["can_admin"], true);
    let acl = h.json(&members, &alice).await;
    assert_eq!(acl["members"], json!([]));
    let grant: Value = repository_write(&h, Method::PUT, &members, &alice, pending.id,
        json!({"expected_revision":acl["revision"],"members":[{"subject":"bob-id","name":"Bob","access":"read"}],"groups":[]}))
        .await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(grant["can_admin"], true);
    let bob_catalog = h.json(&listed, &bob).await;
    assert_eq!(bob_catalog["repositories"][0]["access"], "read");
    assert_eq!(bob_catalog["repositories"][0]["can_admin"], false);

    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":owner,"repository":"widget","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = token["token"].as_str().unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/{owner}/widget.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(Some(secret)).unwrap();
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), remote.as_str()).await;
    assert_eq!(
        h.json(&format!("{repo}/refs"), &alice).await["head"]["name"],
        "refs/heads/main"
    );

    let bob_membership = format!("{root}/members/{bob_id}");
    let bob_version = h.json(&bob_membership, &alice).await["member"]["version"].clone();
    h.write(
        Method::PUT,
        &bob_membership,
        &alice,
        json!({"version":bob_version,"role":"owner"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&listed, &bob).await["repositories"][0]["can_admin"],
        true
    );
    assert_eq!(
        h.write(Method::POST, &creation, &bob, body.clone())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let refs = format!("{repo}/settings/branch-protections");
    repository_write(&h, Method::PUT, &refs, &alice, pending.id,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1,"required_checks":[]}]}))
        .await.error_for_status().unwrap();
    repository_write(&h, Method::POST, &format!("{repo}/pulls"), &alice, pending.id,
        json!({"request_id":Uuid::now_v7(),"title":"Inherited owner review","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    repository_write(
        &h,
        Method::POST,
        &format!("{repo}/pulls/1/reviews"),
        &bob,
        pending.id,
        json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Approved","commit_oid":head}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&format!("{repo}/pulls/1"), &alice).await["merge_requirements"]["approvals"],
        1
    );

    h.write(
        Method::DELETE,
        &format!("{root}/members/{alice_id}"),
        &bob,
        json!({"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(Method::POST, &creation, &alice, body.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.json(&listed, &alice).await["repositories"], json!([]));
    let read = h
        .http
        .get(format!(
            "{}/git/{owner}/widget.git/info/refs?service=git-upload-pack",
            h.origin
        ))
        .basic_auth("crab", Some(secret))
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::NOT_FOUND);
    let acl = h.json(&members, &bob).await;
    repository_write(
        &h,
        Method::PUT,
        &members,
        &bob,
        pending.id,
        json!({"expected_revision":acl["revision"],"members":[],"groups":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let renamed = h.json(&root, &bob).await;
    h.write(Method::PATCH, &root, &bob,
        json!({"version":renamed["organization"]["version"],"name":"Renamed team","description":""}))
        .await.error_for_status().unwrap();
    let visible = h.json(&listed, &bob).await;
    assert_eq!(visible["repositories"][0]["owner"], owner);
    assert_eq!(visible["repositories"][0]["id"], json!(pending.id));
    assert_eq!(visible["repositories"][0]["can_admin"], true);
    h.close().await;
}

#[tokio::test]
async fn repository_team_access_follows_current_membership_and_private_team_visibility() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h =
        Harness::new_with_store_and_prefix(false, false, store, "organization-team-access").await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, bob_id) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let teams = format!("{org}/teams/commands");
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

    let created: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let repo_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let owner = created["owner"].as_str().unwrap();
    let repo = format!("/api/repos/{owner}/library");
    let endpoint = format!("{repo}/team-access");
    let listed = format!("/api/repos?organization={organization}");
    let searched = format!("{listed}&q=LIBRARY&limit=1");
    let team = Uuid::now_v7();
    h.write(Method::POST, &teams, &alice,
        json!({"request_id":team,"command":{"kind":"create","name":"Engineering","description":"","visibility":"secret"}}))
        .await.error_for_status().unwrap();
    h.write(
        Method::POST,
        &teams,
        &alice,
        json!({"request_id":Uuid::now_v7(),"command":{
            "kind":"member","team":team,"member":bob_id,"version":0,"role":"member"
        }}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&listed, &bob).await["repositories"], json!([]));
    assert_eq!(h.json(&searched, &bob).await["repositories"], json!([]));

    let revision = h.json(&endpoint, &alice).await["revision"]
        .as_u64()
        .unwrap();
    let unknown = repository_write(
        &h,
        Method::PUT,
        &endpoint,
        &alice,
        repo_id,
        json!({"expected_revision":revision,"teams":[{"team":Uuid::now_v7(),"access":"write"}]}),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    repository_write(
        &h,
        Method::PUT,
        &endpoint,
        &alice,
        repo_id,
        json!({"expected_revision":revision,"teams":[{"team":team,"access":"write"}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let stale = repository_write(
        &h,
        Method::PUT,
        &endpoint,
        &alice,
        repo_id,
        json!({"expected_revision":revision,"teams":[{"team":team,"access":"read"}]}),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let granted = h.json(&endpoint, &alice).await;
    assert_eq!(
        granted["organization"]["teams"],
        json!([{"team":team,"access":"write"}])
    );
    let last_revision = granted["revision"].as_u64().unwrap().to_string();
    let (document, _) = catalog.load().await.unwrap();
    assert!(
        document
            .membership_audit_head
            .unwrap()
            .contains(&last_revision)
    );
    let visible = h.json(&listed, &bob).await;
    assert_eq!(visible["repositories"][0]["id"], json!(repo_id));
    assert_eq!(visible["repositories"][0]["access"], "write");
    assert_eq!(visible["repositories"][0]["can_admin"], false);
    let matching = h.json(&searched, &bob).await;
    assert_eq!(matching["repositories"][0]["id"], json!(repo_id));
    assert_eq!(matching["next"], Value::Null);

    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &bob,
            json!({"owner":owner,"repository":"library","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let remote = format!("{}/git/{owner}/library.git", h.origin);
    let mut git_url = url::Url::parse(&remote).unwrap();
    git_url.set_username("crab").unwrap();
    git_url
        .set_password(Some(token["token"].as_str().unwrap()))
        .unwrap();
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), git_url.as_str()).await;

    let governed = format!("{repo}/rulesets");
    let choices = h.json(&governed, &alice).await;
    assert_eq!(choices["bypass_options"]["teams"], json!([]));
    let rule = json!({
        "id":Uuid::now_v7(),"name":"Release creation","target":"branch","enforcement":"active",
        "include":["release/**/*"],"exclude":[],"restrict_creations":true,"restrict_deletions":false,
        "require_pull_request":false,"required_approvals":0,"required_code_owner_reviews":false,
        "required_checks":[],"required_integrations":[],
        "bypass_actors":[{"actor":{"kind":"team","id":team},"mode":"always"}]
    });
    let candidate = json!({"expected_version":0,"rulesets":[rule]});
    assert_eq!(
        repository_write(
            &h,
            Method::PUT,
            &governed,
            &alice,
            repo_id,
            candidate.clone()
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
    );
    let team_path = format!("{org}/teams/{team}");
    let version = h.json(&team_path, &alice).await["team"]["version"].clone();
    h.write(Method::POST, &teams, &alice, json!({"request_id":Uuid::now_v7(),"command":{
        "kind":"edit","team":team,"name":"Engineering","description":"","version":version,"visibility":"closed"
    }})).await.error_for_status().unwrap();
    assert_eq!(
        h.json(&governed, &alice).await["bypass_options"]["teams"],
        json!([{"id":team,"name":"Engineering"}])
    );
    repository_write(&h, Method::PUT, &governed, &alice, repo_id, candidate)
        .await
        .error_for_status()
        .unwrap();
    git::success(
        source.path(),
        &[
            "push",
            git_url.as_str(),
            "HEAD:refs/heads/release/team/first",
        ],
    )
    .await;

    let version = h.json(&team_path, &alice).await["team"]["version"].clone();
    h.write(Method::POST, &teams, &alice, json!({"request_id":Uuid::now_v7(),"command":{
        "kind":"edit","team":team,"name":"Engineering","description":"","version":version,"visibility":"secret"
    }})).await.error_for_status().unwrap();
    let blocked = git::git(
        source.path(),
        &[
            "push",
            git_url.as_str(),
            "HEAD:refs/heads/release/team/secret",
        ],
    )
    .await;
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("restricts ref creation"));

    repository_write(&h, Method::PUT, &format!("{repo}/settings/branch-protections"), &alice, repo_id,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1,"required_checks":[]}]}))
        .await.error_for_status().unwrap();
    repository_write(&h, Method::POST, &format!("{repo}/pulls"), &alice, repo_id,
        json!({"request_id":Uuid::now_v7(),"title":"Team review","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    let pull = format!("{repo}/pulls/1");
    let reviewers = format!("{pull}/reviewers");
    let requests = format!("{pull}/review-requests");
    let inbox = format!("/api/notifications?repository={repo_id}");
    let ready = h.json(&reviewers, &alice).await;
    assert_eq!(
        ready["teams"],
        json!([{"id":team,"name":"Engineering","state":null,"can_request":true}])
    );
    let request =
        json!({"version":ready["version"],"change":{"kind":"request_team","team_id":team}});
    for _ in 0..2 {
        repository_write(&h, Method::PUT, &requests, &alice, repo_id, request.clone())
            .await
            .error_for_status()
            .unwrap();
    }
    let search = |surface: &str, query: &str| {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", query)
            .finish();
        format!("{repo}/{surface}?{encoded}")
    };
    let qualifier = format!("team-review-requested:{owner}/engineering");
    let mine = search("pulls", "is:open review-requested:@me");
    let named = search("issues", &format!("is:pr ({qualifier} OR label:missing)"));
    assert_eq!(h.json(&named, &bob).await["items"][0]["number"], 1);
    assert_eq!(h.json(&mine, &bob).await["matching"], 1);
    let direct = search("pulls", "is:open user-review-requested:@me");
    assert_eq!(h.json(&direct, &bob).await["matching"], 0);

    h.deliver_notifications(owner, "library").await;
    let notified = h.json(&inbox, &bob).await;
    assert_eq!(notified["items"].as_array().unwrap().len(), 1);
    assert_eq!(notified["items"][0]["reason"], "review_requested");
    assert!(
        notified["items"][0]["href"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/pull/1?repository_id={repo_id}"))
    );
    assert!(!notified.to_string().contains("Engineering"));
    assert_eq!(h.json(&inbox, &alice).await["items"], json!([]));

    let approval =
        json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Approved","commit_oid":head});
    repository_write(
        &h,
        Method::POST,
        &format!("{pull}/reviews"),
        &bob,
        repo_id,
        approval.clone(),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&pull, &alice).await["merge_requirements"]["approvals"],
        1
    );
    assert_eq!(h.json(&named, &bob).await["matching"], 0);
    assert_eq!(h.json(&mine, &bob).await["matching"], 0);
    let reviewed = h.json(&reviewers, &alice).await;
    assert_eq!(
        reviewed["teams"][0]["state"],
        json!({"kind":"fulfilled","review":1})
    );
    assert_eq!(
        repository_write(&h, Method::PUT, &requests, &alice, repo_id, request)
            .await
            .status(),
        StatusCode::CONFLICT,
    );
    repository_write(
        &h,
        Method::PUT,
        &requests,
        &alice,
        repo_id,
        json!({"version":reviewed["version"],"change":{"kind":"request_team","team_id":team}}),
    )
    .await
    .error_for_status()
    .unwrap();
    repository_write(
        &h,
        Method::POST,
        &format!("{pull}/reviews"),
        &bob,
        repo_id,
        approval,
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&reviewers, &alice).await["teams"][0]["state"],
        json!({"kind":"requested"})
    );

    h.write(
        Method::POST,
        &teams,
        &alice,
        json!({"request_id":Uuid::now_v7(),"command":{
            "kind":"member","team":team,"member":bob_id,"version":1,"role":null
        }}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&listed, &bob).await["repositories"], json!([]));
    assert_eq!(h.json(&searched, &bob).await["repositories"], json!([]));
    assert_eq!(h.json("/api/notifications", &bob).await["items"], json!([]));
    assert_eq!(
        h.json(&pull, &alice).await["merge_requirements"]["approvals"],
        0
    );
    // The original token is still stored, but the live repository grant must
    // disappear for Git reads and writes together with the browser session.
    for service in ["git-upload-pack", "git-receive-pack"] {
        let denied = h
            .http
            .get(format!("{remote}/info/refs?service={service}"))
            .header("git-protocol", "version=2")
            .basic_auth("crab", token["token"].as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    }

    let acl = h.json(&format!("{repo}/members"), &alice).await;
    repository_write(&h, Method::PUT, &format!("{repo}/members"), &alice, repo_id,
        json!({"expected_revision":acl["revision"],"members":[{"subject":"bob-id","name":"Bob","access":"admin"}],"groups":[]}))
        .await.error_for_status().unwrap();
    // Regranting repository access does not restore the removed team bypass.
    let blocked = git::git(
        source.path(),
        &[
            "push",
            git_url.as_str(),
            "HEAD:refs/heads/release/team/after-removal",
        ],
    )
    .await;
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("restricts ref creation"));
    let bob_teams = h
        .http
        .get(format!("{}{endpoint}", h.origin))
        .header("cookie", &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(bob_teams.status(), StatusCode::NOT_FOUND);
    // Bob can read the pull via a direct grant but cannot filter by the
    // secret team, even when the exact team name is already known.
    assert_eq!(h.json(&named, &alice).await["matching"], 1);
    assert_eq!(h.json(&named, &bob).await["matching"], 0);
    assert_eq!(h.json(&mine, &bob).await["matching"], 0);
    let completion =
        format!("{repo}/discussion-search?query=team-review-requested%3Aeng&cursor=25");
    assert_eq!(
        h.json(&completion, &alice).await["items"][0]["description"],
        "Engineering"
    );
    assert_eq!(h.json(&completion, &bob).await["items"], json!([]));
    let bob_reviewers = h.json(&reviewers, &bob).await;
    assert_eq!(bob_reviewers["teams"], json!([]));
    assert_eq!(
        repository_write(&h, Method::PUT, &requests, &bob, repo_id,
            json!({"version":bob_reviewers["version"],"change":{"kind":"cancel_team","team_id":team}}))
            .await.status(),
        StatusCode::NOT_FOUND,
    );
    repository_write(&h, Method::POST, &format!("{pull}/reviews"), &bob, repo_id,
        json!({"request_id":Uuid::now_v7(),"state":"commented","body":"Former team member","commit_oid":head}))
        .await.error_for_status().unwrap();
    let pending = h.json(&reviewers, &alice).await;
    assert_eq!(pending["teams"][0]["state"], json!({"kind":"requested"}));
    repository_write(
        &h,
        Method::PUT,
        &requests,
        &alice,
        repo_id,
        json!({"version":pending["version"],"change":{"kind":"cancel_team","team_id":team}}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&reviewers, &alice).await["teams"][0]["state"],
        Value::Null
    );
    let denied = repository_write(
        &h,
        Method::PUT,
        &endpoint,
        &bob,
        repo_id,
        json!({"expected_revision":acl["revision"],"teams":[]}),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        h.json(&endpoint, &alice).await["organization"]["teams"][0]["access"],
        "write"
    );

    let current_team = h.json(&format!("{org}/teams/{team}"), &alice).await;
    h.write(
        Method::POST,
        &teams,
        &alice,
        json!({"request_id":Uuid::now_v7(),"command":{
            "kind":"delete","team":team,"version":current_team["team"]["version"]
        }}),
    )
    .await
    .error_for_status()
    .unwrap();
    let deleted = h.json(&endpoint, &alice).await;
    assert!(deleted["team_names"].get(team.to_string()).is_none());
    assert_eq!(
        deleted["organization"]["teams"],
        json!([{"team":team,"access":"write"}])
    );
    repository_write(
        &h,
        Method::PUT,
        &format!("{repo}/settings/archive"),
        &alice,
        repo_id,
        json!({"expected_version":0,"archived":true,"repository":format!("{owner}/library")}),
    )
    .await
    .error_for_status()
    .unwrap();
    repository_write(
        &h,
        Method::PUT,
        &endpoint,
        &alice,
        repo_id,
        json!({"expected_revision":deleted["revision"],"teams":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&endpoint, &alice).await["organization"]["teams"],
        json!([])
    );
    h.close().await;
}
