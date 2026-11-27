use super::*;

#[tokio::test]
async fn pull_only_subscription_delivers_every_committed_head_and_stable_redelivery() {
    let (endpoint, received, target) = receiver(false).await;
    let h = Harness::with_webhook_receivers([endpoint], None).await;
    let alice = h.login().await;
    let head = stock_push(&h, &alice).await;
    let checkout = checkout_feature(&h, &alice).await;
    git::success(
        checkout.path(),
        &[
            "push",
            "origin",
            "refs/remotes/origin/main:refs/heads/reopened-base",
            "refs/remotes/origin/main:refs/heads/late-base",
        ],
    )
    .await;
    let response: Value = h
        .write(Method::POST, PATH, &alice, configure())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = response["secret"].as_str().unwrap();
    let first = json!({"request_id":Uuid::now_v7(),"title":"Track feature","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"});
    open(&h, &alice, &first).await;
    let reopened = json!({"request_id":Uuid::now_v7(),"title":"Reopen after the push","body":"","base_ref":"refs/heads/reopened-base","head_ref":"refs/heads/feature"});
    open(&h, &alice, &reopened).await;
    drain(&h).await;

    let next = advance_feature(checkout.path(), "origin").await;
    let newest = advance_feature(checkout.path(), "origin").await;
    for (version, state) in [(1, "closed"), (2, "open")] {
        h.write(
            Method::PATCH,
            "/api/repos/team/private/pulls/2",
            &alice,
            json!({"version":version,"state":state}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let late = json!({"request_id":Uuid::now_v7(),"title":"Track future updates","body":"","base_ref":"refs/heads/late-base","head_ref":"refs/heads/feature"});
    open(&h, &alice, &late).await;
    let repository = &h.server.repositories.values()[0];
    let bogus = crab_git::receive_plan::RefUpdate {
        name: "refs/heads/feature".into(),
        old: Some(newest.parse().unwrap()),
        new: Some(head.parse().unwrap()),
    };
    crate::server::webhooks::reserve_publication(&h.server, repository, &"5".repeat(64), &[bogus])
        .await
        .unwrap();
    drain(&h).await;

    let rows = received.lock().await.clone();
    assert_eq!(rows.len(), 7);
    assert!(rows.iter().all(|row| row.event == "pull_request"));
    let synchronize: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            let body: Value = serde_json::from_slice(&row.body).unwrap();
            (body["data"]["action"] == "synchronize").then_some((row, body))
        })
        .collect();
    assert_eq!(synchronize.len(), 2);
    for (old, new) in [(&head, &next), (&next, &newest)] {
        let (row, body) = synchronize
            .iter()
            .find(|(_, body)| body["data"]["head_sha"] == *new)
            .unwrap();
        assert_eq!(body["data"]["number"], 1);
        assert_eq!(
            body["data"]["head_change"],
            json!({"before":old,"after":new})
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&row.body);
        let expected = blake3::Hash::from_bytes(mac.finalize().into_bytes().into());
        assert_eq!(row.signature, format!("sha256={expected}"));
    }

    let (row, _) = synchronize
        .iter()
        .find(|(_, body)| body["data"]["head_sha"] == next)
        .unwrap();
    let history = h.json(PATH, &alice).await;
    let record = history["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == json!(row.id))
        .unwrap();
    assert_eq!(record["head_change"], json!({"before":head,"after":next}));
    h.write(
        Method::POST,
        PATH,
        &alice,
        json!({"kind":"redeliver","id":row.id,"version":record["version"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    drain(&h).await;
    assert_eq!(received.lock().await.last(), Some(*row));
    h.close().await;
    target.abort();
    let _ = target.await;
}

fn configure() -> Value {
    json!({"kind":"configure","id":Uuid::now_v7(),"expected":null,"push":false,"pull_request":true})
}

async fn write(
    h: &Harness,
    account: &str,
    repository: Uuid,
    path: &str,
    body: Value,
) -> reqwest::Response {
    let session = h.json("/api/session", account).await;
    h.http
        .post(format!("{}{path}", h.origin))
        .header("cookie", account)
        .header("origin", &h.origin)
        .header("x-crab-repository-id", repository.to_string())
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn private_fork_push_synchronizes_only_the_matching_upstream_pull() {
    use crate::catalog::{BrowserNamespace, MembershipActor, RepositoryApplicationState};

    let (endpoint, received, target) = receiver(false).await;
    let upstream_id = Uuid::now_v7();
    let h = Harness::with_webhook_receivers([endpoint], Some(upstream_id)).await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    crate::test_support::http::organizations::set_private_forks(&h, organization, &alice, true)
        .await;
    let upstream = catalog
        .reserve_git_import(
            BrowserNamespace::Organization {
                id: organization,
                owner: crate::catalog::derive_organization_handle("org", organization),
            },
            crate::catalog::GitImportReservation {
                id: upstream_id,
                name: "upstream".into(),
                description: String::new(),
                visibility: crate::catalog::visibility::Visibility::Private,
                forked_from: None,
                mirror: None,
            },
            MembershipActor {
                issuer: h.provider.issuer.clone(),
                subject: "alice-id".into(),
            },
            vec![],
        )
        .await
        .unwrap();
    catalog
        .initialize_git(&upstream, "main", true)
        .await
        .unwrap();
    h.server
        .initialize_catalog_repository(upstream_id, RepositoryApplicationState::GitImportPending)
        .await
        .unwrap();
    h.server
        .install_catalog(catalog.load().await.unwrap().0)
        .await
        .unwrap();

    let owner = &upstream.owner;
    let route = format!("/api/repos/{owner}/upstream");
    let hooks = format!("{route}/settings/webhooks");
    let remote = git_remote(&h, &alice, owner, "upstream").await;
    let checkout = tempfile::tempdir().unwrap();
    let before = git::push_review_branches(checkout.path(), remote.as_str()).await;
    let base = git::success(checkout.path(), &["rev-parse", "main"]).await;
    let request = Uuid::now_v7();
    assert_eq!(
        write(
            &h,
            &alice,
            upstream_id,
            &format!("{route}/forks"),
            json!({"request_id":request,"name":"fork"})
        )
        .await
        .status(),
        StatusCode::ACCEPTED,
    );
    let status_path =
        format!("/api/organizations/{organization}/repositories/imports/git/{request}");
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let status = h.json(&status_path, &alice).await;
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
    let fork = h
        .server
        .repositories
        .get(&(owner.to_owned(), "fork".into()))
        .unwrap();
    let fork_remote = git_remote(&h, &alice, owner, "fork").await;
    write(&h, &alice, upstream_id, &hooks, configure())
        .await
        .error_for_status()
        .unwrap();
    let pulls = format!("{route}/pulls");
    let pull_request = |source: Option<Uuid>| {
        json!({"request_id":Uuid::now_v7(),"title":"Verify head source","body":"","base_ref":"refs/heads/main",
            "head_ref":"refs/heads/feature","head_repository":source,"expected":{"base_oid":base,"head_oid":before}})
    };
    for source in [None, Some(fork.id)] {
        write(&h, &alice, upstream_id, &pulls, pull_request(source))
            .await
            .error_for_status()
            .unwrap();
    }
    let after = advance_feature(checkout.path(), fork_remote.as_str()).await;
    let repo = h.server.repositories.by_id(upstream_id).unwrap();
    crate::server::webhooks::drain(&h.server, &repo, h.server.webhook_sender.as_ref().unwrap())
        .await
        .unwrap();
    let rows = received.lock().await.clone();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.event == "pull_request"));
    let events: Vec<Value> = rows
        .iter()
        .map(|row| serde_json::from_slice(&row.body).unwrap())
        .collect();
    let changes: Vec<_> = events
        .iter()
        .filter(|body| body["data"]["action"] == "synchronize")
        .collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["repository"], json!(upstream_id));
    assert_eq!(changes[0]["data"]["number"], 2);
    assert_eq!(changes[0]["data"]["head_repository"], json!(fork.id));
    assert_eq!(
        changes[0]["data"]["head_change"],
        json!({"before":before,"after":after})
    );
    assert_eq!(
        h.json(&format!("{pulls}/2"), &alice).await["head_oid"],
        after
    );
    h.close().await;
    target.abort();
    let _ = target.await;
}
