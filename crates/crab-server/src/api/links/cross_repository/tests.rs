use super::*;
use crate::{
    RepositoryAccess, RepositoryMember,
    cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor},
    test_support::{
        git,
        http::{Harness, browser::Browser, organizations},
    },
};
use reqwest::{Method, StatusCode, header};

mod commit_messages;
mod keywords;

struct Repo {
    id: Uuid,
    owner: String,
    name: String,
}

impl Repo {
    fn path(&self) -> String {
        format!("/{}/{}", self.owner, self.name)
    }

    fn api(&self, suffix: &str) -> String {
        format!("/api/repos{}{suffix}", self.path())
    }

    async fn create(h: &Harness, alice: &str, org: Uuid, name: &str) -> Self {
        let result: Value = h
            .write(
                Method::POST,
                &format!("/api/organizations/{org}/repositories/creation"),
                alice,
                json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        Self {
            id: Uuid::parse_str(result["id"].as_str().unwrap()).unwrap(),
            owner: result["owner"].as_str().unwrap().into(),
            name: name.into(),
        }
    }

    async fn write(
        &self,
        h: &Harness,
        method: Method,
        suffix: &str,
        cookie: &str,
        body: Value,
    ) -> reqwest::Response {
        let session = h.json("/api/session", cookie).await;
        h.http
            .request(method, format!("{}{}", h.origin, self.api(suffix)))
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header("x-crab-repository-id", self.id.to_string())
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn set_bob_access(&self, h: &Harness, access: Option<RepositoryAccess>) {
        let mut members = vec![RepositoryMember {
            subject: "alice-id".into(),
            name: "Alice".into(),
            access: RepositoryAccess::Admin,
        }];
        if let Some(access) = access {
            members.push(RepositoryMember {
                subject: "bob-id".into(),
                name: "Bob".into(),
                access,
            });
        }
        h.server
            .catalog()
            .unwrap()
            .set_members(&self.owner, &self.name, members, true)
            .await
            .unwrap();
    }
}

async fn setup() -> (Harness, String, Uuid, Repo, Repo) {
    let backing = std::sync::Arc::new(object_store::memory::InMemory::new());
    setup_on(crab_storage::Store::new(backing)).await
}

async fn setup_on(store: crab_storage::Store) -> (Harness, String, Uuid, Repo, Repo) {
    crate::test_support::server::init_test_tracing();
    let h = Harness::new_with_store_and_prefix(false, false, store, "linked-work").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (org, _, _) = organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let issue = Repo::create(&h, &alice, org, "rollout").await;
    let pull = Repo::create(&h, &alice, org, "service").await;
    issue.write(
        &h,
        Method::POST,
        "/issues",
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Coordinate rollout","body":"Private issue body"}),
    )
    .await
    .error_for_status()
    .unwrap();
    (h, alice, org, issue, pull)
}

async fn wait_for_transfer(h: &Harness, original: &str, actor: &str) -> u64 {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let issue = h.json(original, actor).await;
            if issue["transfer"]["state"] == "ready" {
                break issue["transfer"]["number"].as_u64().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap()
}

async fn set_issue_state(h: &Harness, actor: &str, repo: &Repo, number: u64, state: &str) {
    let path = format!("/issues/{number}");
    let version = h.json(&repo.api(&path), actor).await["version"].clone();
    repo.write(
        h,
        Method::PATCH,
        &path,
        actor,
        json!({"version":version,"state":state}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn fixture() -> (Harness, String, Uuid, Repo, Repo) {
    let (h, alice, org, issue, pull) = setup().await;
    let cell = h.server.cells().unwrap().repository(pull.id).unwrap();
    cell.client
        .command::<CreatePull>(
            cell.target,
            app::mutation_identity().unwrap(),
            CreatePullInput {
                code_owner_requests: Default::default(),
                metadata: Default::default(),
                draft: false,
                submission_id: *Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                title: "Ship the service".into(),
                body: "Private pull body".into(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                head_repository: None,
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    (h, alice, org, issue, pull)
}

async fn git_pull(
    h: &Harness,
    actor: &str,
    repo: &Repo,
    checkout: &std::path::Path,
    description: &str,
) -> (Value, url::Url) {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            actor,
            json!({"owner":repo.owner,"repository":repo.name,"access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git{}.git", h.origin, repo.path())).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let head = git::push_review_branches(checkout, remote.as_str()).await;
    let base = git::success(checkout, &["rev-parse", "main"]).await;
    let pull: Value = repo
        .write(
            h,
            Method::POST,
            "/pulls",
            actor,
            json!({
                "request_id":Uuid::now_v7(),"title":"Ship the service","body":description,
                "base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
                "expected":{"base_oid":base,"head_oid":head},
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    (pull, remote)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_default_branch_merge_closes_an_authorized_cross_repository_issue_once() {
    use std::sync::atomic::Ordering;
    let (h, alice, organization, issue, pull) = setup().await;
    let restricted = Repo::create(&h, &alice, organization, "customer").await;
    restricted
        .write(
            &h,
            Method::POST,
            "/issues",
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Customer rollout","body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (opened, remote) = git_pull(&h, &alice, &pull, checkout.path(), "").await;
    let selection = format!("/issues/1/linked-pulls/{}/1", pull.id);
    let selected = json!({"active":true,"version":0,"closes_on_merge":true});
    for _ in 0..2 {
        let reply: Value = issue
            .write(&h, Method::PUT, &selection, &alice, selected.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            reply,
            json!({"active":true,"version":1,"closes_on_merge":true})
        );
    }
    restricted
        .write(&h, Method::PUT, &selection, &alice, selected)
        .await
        .error_for_status()
        .unwrap();
    for repo in [&issue, &pull] {
        repo.set_bob_access(&h, Some(RepositoryAccess::Write)).await;
    }
    restricted
        .set_bob_access(&h, Some(RepositoryAccess::Read))
        .await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let merge = json!({
        "request_id":Uuid::now_v7(),"version":opened["version"],"method":"squash",
        "base_oid":opened["base_oid"],"head_oid":opened["head_oid"],
    });
    h.drop_next_merge_reply.store(true, Ordering::SeqCst);
    let result = pull
        .write(&h, Method::POST, "/pulls/1/merge", &bob, merge.clone())
        .await;
    assert_eq!(
        result.status(),
        StatusCode::OK,
        "{}",
        result.text().await.unwrap()
    );
    assert!(!h.drop_next_merge_reply.load(Ordering::SeqCst));
    let issue_path = issue.api("/issues/1");
    let closed = h.json(&issue_path, &alice).await;
    assert_eq!(closed["state_reason"], "completed");
    assert_eq!(
        h.json(&restricted.api("/issues/1"), &alice).await["state"],
        "open"
    );
    assert_eq!(
        h.json(&pull.api("/pulls/1"), &alice).await["state"],
        "merged"
    );
    set_issue_state(&h, &alice, &issue, 1, "open").await;

    advance_default(checkout.path(), &remote).await;
    let recovered: Value = pull
        .write(&h, Method::POST, "/pulls/1/merge", &alice, merge)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(recovered["state"], "merged");
    assert_eq!(h.json(&issue_path, &alice).await["state"], "open");
    h.close().await;
}

async fn advance_default(checkout: &std::path::Path, remote: &url::Url) {
    git::success(checkout, &["fetch", remote.as_str(), "main"]).await;
    git::success(checkout, &["checkout", "--detach", "FETCH_HEAD"]).await;
    git::success(
        checkout,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "Advance the default branch",
        ],
    )
    .await;
    git::success(checkout, &["push", remote.as_str(), "HEAD:refs/heads/main"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_repository_closure_recovers_after_issue_transfer_and_branch_movement() {
    let (store, disconnect) = crate::git::receive::fault_plan_receipt();
    let (h, alice, organization, issue, pull) = setup_on(store).await;
    let tracking = Repo::create(&h, &alice, organization, "tracking").await;
    set_issue_state(&h, &alice, &issue, 1, "closed").await;
    set_issue_state(&h, &alice, &issue, 1, "open").await;
    issue.write(&h, Method::POST, "/issues", &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Keep open after reconsideration","body":""}))
        .await.error_for_status().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (opened, remote) = git_pull(&h, &alice, &pull, checkout.path(), "").await;
    for number in [1, 2] {
        let selection = format!("/issues/{number}/linked-pulls/{}/1", pull.id);
        issue
            .write(
                &h,
                Method::PUT,
                &selection,
                &alice,
                json!({"active":true,"version":0,"closes_on_merge":true}),
            )
            .await
            .error_for_status()
            .unwrap();
    }
    let repository = h.server.repositories.by_id(pull.id).unwrap();
    let merge = json!({
        "request_id":Uuid::now_v7(),"version":opened["version"],"method":"squash",
        "base_oid":opened["base_oid"],"head_oid":opened["head_oid"],
    });
    disconnect(true);
    let _reply = pull
        .write(&h, Method::POST, "/pulls/1/merge", &alice, merge.clone())
        .await;
    let published = repository
        .ref_oid("refs/heads/main")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(Some(published.as_str()), opened["base_oid"].as_str());
    let original = issue.api("/issues/1");
    let before = h.json(&original, &alice).await;
    assert_eq!(before["state"], "open");
    set_issue_state(&h, &alice, &issue, 2, "closed").await;
    set_issue_state(&h, &alice, &issue, 2, "open").await;

    let reply = issue.write(&h, Method::POST, "/issues/1/transfer", &alice,
        json!({"request_id":Uuid::now_v7(),"version":before["version"],"destination":tracking.id})).await;
    assert_eq!(
        reply.status(),
        StatusCode::ACCEPTED,
        "{}",
        reply.text().await.unwrap()
    );
    let moved = format!("/issues/{}", wait_for_transfer(&h, &original, &alice).await);
    tracking
        .write(
            &h,
            Method::PATCH,
            &moved,
            &alice,
            json!({"version":before["version"],"title":"Updated rollout"}),
        )
        .await
        .error_for_status()
        .unwrap();

    disconnect(false);
    advance_default(checkout.path(), &remote).await;
    assert_ne!(
        repository
            .ref_oid("refs/heads/main")
            .await
            .unwrap()
            .as_deref(),
        Some(published.as_str())
    );
    assert_eq!(h.json(&tracking.api(&moved), &alice).await["state"], "open");
    crate::repository::commit_closures::recover(&h.server, &repository)
        .await
        .unwrap();
    let closed = h.json(&tracking.api(&moved), &alice).await;
    assert_eq!(closed["state_reason"], "completed");
    assert_eq!(closed["title"], "Updated rollout");
    assert_eq!(
        h.json(&issue.api("/issues/2"), &alice).await["state"],
        "open"
    );
    let recovered: Value = pull
        .write(&h, Method::POST, "/pulls/1/merge", &alice, merge)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(recovered["state"], "merged");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_repository_links_survive_issue_transfer_and_require_live_grants_on_both_sides() {
    let (h, alice, organization, issue, pull) = fixture().await;
    let destination = Repo::create(&h, &alice, organization, "tracking").await;
    let selection = format!("/issues/1/linked-pulls/{}/1", pull.id);
    let issue_links = issue.api("/issues/1/links");
    let pull_links = pull.api("/pulls/1/links");
    let link = json!({"active":true,"version":0});
    assert_eq!(
        h.json(&issue.api(&selection), &alice).await,
        json!({"active":false,"version":0})
    );

    issue.set_bob_access(&h, Some(RepositoryAccess::Read)).await;
    pull.set_bob_access(&h, Some(RepositoryAccess::Read)).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        issue
            .write(&h, Method::PUT, &selection, &bob, link.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        let result: Value = issue
            .write(&h, Method::PUT, &selection, &alice, link.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result, json!({"active":true,"version":1}));
    }
    let related = h.json(&issue_links, &bob).await;
    assert_eq!(
        related["cross_repository"][0]["work"]["title"],
        "Ship the service"
    );
    assert!(!related.to_string().contains("Private pull body"));
    let related = h.json(&pull_links, &bob).await;
    assert_eq!(
        related["cross_repository"][0]["work"]["title"],
        "Coordinate rollout"
    );
    assert_eq!(related["cross_repository"][0]["can_unlink"], false);

    pull.set_bob_access(&h, None).await;
    assert_eq!(
        h.json(&issue_links, &bob).await["cross_repository"],
        json!([])
    );
    assert_eq!(
        h.http
            .get(format!("{}{}", h.origin, issue.api(&selection)))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    pull.set_bob_access(&h, Some(RepositoryAccess::Read)).await;

    let before = h.json(&issue.api("/issues/1"), &alice).await;
    let reply = issue
        .write(
            &h,
            Method::POST,
            "/issues/1/transfer",
            &alice,
            json!({"request_id":Uuid::now_v7(),"version":before["version"],"destination":destination.id}),
        )
        .await;
    assert_eq!(
        reply.status(),
        StatusCode::ACCEPTED,
        "{}",
        reply.text().await.unwrap()
    );
    let moved_number = wait_for_transfer(&h, &issue.api("/issues/1"), &alice).await;
    let related = h.json(&pull_links, &alice).await;
    assert_eq!(
        related["cross_repository"][0]["repository"]["id"],
        json!(destination.id)
    );
    assert_eq!(
        related["cross_repository"][0]["work"]["subject"]["number"],
        moved_number
    );
    assert_eq!(related["cross_repository"][0]["can_unlink"], true);
    assert_eq!(
        h.json(&pull_links, &bob).await["cross_repository"],
        json!([])
    );
    destination
        .set_bob_access(&h, Some(RepositoryAccess::Read))
        .await;
    issue.set_bob_access(&h, None).await;
    assert_eq!(
        h.json(&pull_links, &bob).await["cross_repository"][0]["repository"]["id"],
        json!(destination.id)
    );

    let moved_links = destination.api(&format!("/issues/{moved_number}/links"));
    assert_eq!(
        h.json(&moved_links, &alice).await["cross_repository"][0]["repository"]["id"],
        json!(pull.id)
    );
    let moved_selection = format!("/issues/{moved_number}/linked-pulls/{}/1", pull.id);
    destination
        .write(
            &h,
            Method::PUT,
            &moved_selection,
            &alice,
            json!({"active":false,"version":1}),
        )
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.json(&moved_links, &alice).await["cross_repository"],
        json!([])
    );
    assert_eq!(
        h.json(&pull_links, &alice).await["cross_repository"],
        json!([])
    );
    assert_eq!(
        destination
            .write(&h, Method::PUT, &moved_selection, &alice, link)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn browser_links_a_pull_from_another_repository_and_unlinks_it_from_the_pull() {
    let (h, alice, _, issue, pull) = fixture().await;
    let browser = Browser::new("cross-links");
    browser
        .run(&["open", &format!("{}{}/issues/1", h.origin, issue.path())])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Link pull request"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Link pull request",
            "--exact",
        ])
        .await;
    browser
        .choose_select("#linked-pull-repository", &pull.id.to_string())
        .await;
    browser
        .run(&[
            "fill",
            "[role=dialog] input[placeholder^='Search title']",
            "#1",
        ])
        .await;
    browser.run(&["wait", "--text", "Ship the service"]).await;
    browser
        .run(&["find", "text", "Ship the service", "click", "--exact"])
        .await;
    browser
        .run(&["wait", "--fn", "document.getElementById('linked-pull-close-on-merge')?.getAttribute('aria-checked') === 'true'"])
        .await;
    browser
        .run(&["wait", "[role=dialog] button[type=submit]:enabled"])
        .await;
    browser
        .run(&["click", "[role=dialog] button[type=submit]:enabled"])
        .await;
    let pull_title = format!("{}/{}#1 Ship the service", pull.owner, pull.name);
    browser.run(&["wait", "--text", &pull_title]).await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            &pull_title,
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", &format!("**{}/pull/1*", pull.path())])
        .await;
    let issue_title = format!("{}/{}#1", issue.owner, issue.name);
    browser
        .run(&[
            "wait",
            "--text",
            &format!("{issue_title} Coordinate rollout"),
        ])
        .await;
    browser
        .run(&["wait", "--text", "Closes on default-branch merge"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            &format!("Keep open on merge {issue_title}"),
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Close on merge"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            &format!("Unlink {issue_title}"),
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "No linked issues."]).await;
    assert_eq!(
        h.json(&issue.api("/issues/1/links"), &alice).await["cross_repository"],
        json!([])
    );
    drop(browser);
    h.close().await;
}
