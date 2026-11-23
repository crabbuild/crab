use super::{repositories::repository_write, repository_access::AccessFixture, *};
use crate::test_support::{git, http::browser::Browser};
use sha2::{Digest, Sha256};

async fn repositories(f: &AccessFixture) -> (Value, Value) {
    let listing =
        f.h.json(
            &format!("/api/repos?organization={}", f.organization),
            &f.alice,
        )
        .await;
    let rows = listing["repositories"].as_array().unwrap();
    let named = |name| {
        rows.iter()
            .find(|repo| repo["name"] == name)
            .unwrap()
            .clone()
    };
    (named("library"), named("private-tool"))
}

fn git_url(f: &AccessFixture, entry: &Value) -> String {
    format!(
        "{}/git/{}/{}.git",
        f.h.origin,
        entry["owner"].as_str().unwrap(),
        entry["name"].as_str().unwrap()
    )
}

async fn change(f: &AccessFixture, version: u64, edit: Value) -> Value {
    f.h.write(
        Method::POST,
        &format!("{}/automation", f.org),
        &f.alice,
        json!({"expected_version":version,"edit":edit}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

fn token(issued: &Value) -> &str {
    issued["token"].as_str().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn automation_only_writes_its_grants_and_old_tokens_cannot_regain_access() {
    let f = AccessFixture::new().await;
    let endpoint = format!("{}/automation", f.org);
    let (library, tool) = repositories(&f).await;
    let lib = git_url(&f, &library);
    let second = git_url(&f, &tool);
    let outsider = format!("{}/git/team/private.git", f.h.origin);
    let listed = f.get(&endpoint, &f.bob).await;
    assert_eq!(listed.status(), StatusCode::NOT_FOUND);
    let grants = |library_access| {
        json!([
            {"repository":library["id"],"access":library_access},
            {"repository":tool["id"],"access":"read"},
        ])
    };
    let created = change(
        &f,
        0,
        json!({"kind":"create","name":"Release pipeline", "grants":grants("write")}),
    )
    .await;
    let identity = created["identity"].clone();
    let issued = change(&f, 1, json!({"kind":"issue","identity":identity,"hours":1})).await;
    let writer = token(&issued);
    let snapshot = f.h.json(&endpoint, &f.alice).await;
    assert!(!snapshot.to_string().contains(writer));
    let work = tempfile::tempdir().unwrap();
    let directory = work.path();
    git::success(
        directory,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(directory.join("README.md"), "automation proof\n").unwrap();
    git::success(directory, &["add", "README.md"]).await;
    git::success(
        directory,
        &["commit", "-m", "Seed from the release pipeline"],
    )
    .await;
    assert!(git::with_http_token(directory, writer, &["push", &lib, "main"]).await);
    assert!(git::with_http_token(directory, writer, &["clone", &lib, "automation-clone"]).await);
    assert_eq!(
        std::fs::read_to_string(directory.join("automation-clone/README.md")).unwrap(),
        "automation proof\n"
    );
    assert!(git::with_http_token(directory, writer, &["ls-remote", &second]).await);
    assert!(!git::with_http_token(directory, writer, &["push", &second, "main"]).await);
    assert!(!git::with_http_token(directory, writer, &["ls-remote", &outsider]).await);
    let api = format!("/api/repos/{}/library", library["owner"].as_str().unwrap());
    let rest =
        f.h.http
            .get(format!("{}{api}/pulls", f.h.origin))
            .basic_auth("crab", Some(writer))
            .send()
            .await
            .unwrap();
    assert_eq!(rest.status(), StatusCode::UNAUTHORIZED);

    let content = b"native LFS automation";
    let oid = format!("{:x}", Sha256::digest(content));
    let object = format!("{lib}/info/lfs/objects/{oid}?size={}", content.len());
    f.h.http
        .put(&object)
        .basic_auth("crab", Some(writer))
        .body(content.as_slice())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let read =
        f.h.http
            .get(&object)
            .basic_auth("crab", Some(writer))
            .send()
            .await
            .unwrap();
    assert_eq!(
        read.error_for_status().unwrap().bytes().await.unwrap(),
        content.as_slice()
    );

    let id = Uuid::parse_str(library["id"].as_str().unwrap()).unwrap();
    repository_write(&f.h, Method::PUT, &format!("{api}/settings/branch-protections"), &f.alice, id,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1,"required_checks":[]}]}))
        .await.error_for_status().unwrap();
    git::success(directory, &["commit", "--allow-empty", "-m", "next change"]).await;
    assert!(!git::with_http_token(directory, writer, &["push", &lib, "main"]).await);
    assert!(git::with_http_token(directory, writer, &["push", &lib, "HEAD:refs/heads/ci"]).await);

    change(&f, 2, json!({"kind":"configure","identity":identity,"name":"Release pipeline","grants":grants("read")})).await;
    change(&f, 3, json!({"kind":"configure","identity":identity,"name":"Release pipeline","grants":grants("write")})).await;
    assert!(
        !git::with_http_token(
            directory,
            writer,
            &["push", &lib, "HEAD:refs/heads/old-writer"]
        )
        .await
    );
    assert!(git::with_http_token(directory, writer, &["ls-remote", &lib]).await);
    let next = change(
        &f,
        4,
        json!({"kind":"issue","identity":identity,"hours":24}),
    )
    .await;
    assert!(
        git::with_http_token(
            directory,
            token(&next),
            &["push", &lib, "HEAD:refs/heads/new-writer"]
        )
        .await
    );
    change(&f, 5, json!({"kind":"revoke","identity":identity,"token":snapshot["identities"][0]["tokens"][0]["id"]})).await;
    assert!(!git::with_http_token(directory, writer, &["ls-remote", &lib]).await);
    assert!(git::with_http_token(directory, token(&next), &["ls-remote", &lib]).await);
    change(
        &f,
        6,
        json!({"kind":"enabled","identity":identity,"enabled":false}),
    )
    .await;
    change(
        &f,
        7,
        json!({"kind":"enabled","identity":identity,"enabled":true}),
    )
    .await;
    assert!(!git::with_http_token(directory, token(&next), &["ls-remote", &lib]).await);
    let audit =
        f.h.json(
            &format!("{}/audit-log?action=automation_changed", f.org),
            &f.alice,
        )
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 8);
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI with native Git and Cells"]
async fn native_browser_issues_an_organization_automation_credential() {
    let f = AccessFixture::new().await;
    let (library, _) = repositories(&f).await;
    *f.h.provider.mode.lock().await = String::new();
    let browser = Browser::new("automation");
    browser
        .run(&[
            "open",
            &format!(
                "{}/-/organizations/{}?tab=automation",
                f.h.origin, f.organization
            ),
        ])
        .await;
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    let click = async |name: &str| {
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    browser.run(&["wait","--fn","[...document.querySelectorAll('button')].some(el => el.textContent === 'New automation identity' && !el.disabled)"]).await;
    click("New automation identity").await;
    browser.run(&["wait", "#automation-name"]).await;
    browser
        .run(&["fill", "#automation-name", "Release bot"])
        .await;
    browser.run(&["wait", "--text", "Add library"]).await;
    click("Add library").await;
    click("Create identity").await;
    browser.run(&["wait", "--text", "Issue token"]).await;
    click("Issue token").await;
    browser.run(&["wait", "#automation-secret"]).await;
    let secret = browser
        .run(&["eval", "document.getElementById('automation-secret').value"])
        .await;
    let work = tempfile::tempdir().unwrap();
    let remote = git_url(&f, &library);
    assert!(
        git::with_http_token(
            work.path(),
            secret.trim().trim_matches('"'),
            &["ls-remote", &remote]
        )
        .await
    );
    click("Done").await;
    click("Disable").await;
    click("Confirm").await;
    browser
        .run(&["wait", "--text", "Automation disabled and tokens revoked."])
        .await;
    assert!(
        !git::with_http_token(
            work.path(),
            secret.trim().trim_matches('"'),
            &["ls-remote", &remote]
        )
        .await
    );
    browser.run(&["close"]).await;
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automation_follows_renames_and_a_returned_repository_requires_a_new_grant() {
    use crate::catalog::{
        Rename,
        transfer::{Decision, Proposal, Recipient},
    };
    let f = AccessFixture::new().await;
    let (library, _) = repositories(&f).await;
    let repository = Uuid::parse_str(library["id"].as_str().unwrap()).unwrap();
    let created = change(
        &f,
        0,
        json!({
            "kind":"create","name":"Deployment identity",
            "grants":[{"repository":repository,"access":"read"}],
        }),
    )
    .await;
    let identity = created["identity"].clone();
    let issued = change(&f, 1, json!({"kind":"issue","identity":identity,"hours":1})).await;
    let secret = token(&issued);
    let workspace = tempfile::tempdir().unwrap();
    let initial = git_url(&f, &library);
    assert!(git::with_http_token(workspace.path(), secret, &["ls-remote", &initial]).await);

    let catalog = f.h.server.catalog().unwrap();
    let (current, _) = catalog.load_snapshot().await.unwrap();
    let renamed = catalog
        .rename_repository(
            current.version,
            Rename {
                repository,
                request: Uuid::now_v7(),
                expected_version: 0,
                name: "renamed-library".into(),
            },
        )
        .await
        .unwrap();
    f.h.server.install_catalog(renamed).await.unwrap();
    let updated = format!(
        "{}/git/{}/renamed-library.git",
        f.h.origin,
        library["owner"].as_str().unwrap()
    );
    assert!(git::with_http_token(workspace.path(), secret, &["ls-remote", &updated]).await);

    let (destination, _, _) = crate::test_support::http::organizations::bootstrap(&f.h).await;
    let person = crate::catalog::MembershipActor {
        issuer: f.h.provider.issuer.clone(),
        subject: "alice-id".into(),
    };
    let replay = async |target: Uuid, name: &str| {
        let (state, _) = catalog.load_snapshot().await.unwrap();
        let offer = Uuid::now_v7();
        let now = 1_800_000_000_000;
        let pending = catalog
            .propose_transfer(
                state.version,
                repository,
                Proposal {
                    id: offer,
                    requester: person.clone(),
                    recipient: Recipient::Organization { id: target },
                    recipient_label: "Qualification destination".into(),
                    name: name.into(),
                    version: state.repository_by_id(repository).unwrap().name_version(),
                },
                now,
            )
            .await
            .unwrap();
        let accepted = catalog
            .decide_transfer(
                pending.version,
                offer,
                Decision::Accept {
                    actor: person.clone(),
                    display_name: "Alice".into(),
                },
                now + 100,
            )
            .await
            .unwrap();
        f.h.server.install_catalog(accepted).await.unwrap();
    };
    replay(destination, "away").await;
    replay(f.organization, "renamed-library").await;
    assert!(!git::with_http_token(workspace.path(), secret, &["ls-remote", &updated]).await);
    let path = format!("{}/automation", f.org);
    let snapshot = f.h.json(&path, &f.alice).await;
    assert_eq!(snapshot["identities"][0]["grants"][0]["current"], false);
    assert_eq!(
        f.h.write(
            Method::POST,
            &path,
            &f.alice,
            json!({"expected_version":2,"edit":{"kind":"issue","identity":identity,"hours":1}})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    change(
        &f,
        2,
        json!({
            "kind":"configure","identity":identity,"name":"Deployment identity",
            "grants":[{"repository":repository,"access":"read"}],
        }),
    )
    .await;
    let replacement = change(&f, 3, json!({"kind":"issue","identity":identity,"hours":1})).await;
    assert!(!git::with_http_token(workspace.path(), secret, &["ls-remote", &updated]).await);
    assert!(
        git::with_http_token(
            workspace.path(),
            token(&replacement),
            &["ls-remote", &updated]
        )
        .await
    );
    f.h.close().await;
}
