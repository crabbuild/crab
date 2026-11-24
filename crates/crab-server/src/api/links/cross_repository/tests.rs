use super::*;
use crate::{
    RepositoryAccess, RepositoryMember,
    cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor},
    test_support::http::{Harness, browser::Browser, organizations},
};
use reqwest::{Method, StatusCode, header};

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

    async fn bob_can_read(&self, h: &Harness, read: bool) {
        let mut members = vec![RepositoryMember {
            subject: "alice-id".into(),
            name: "Alice".into(),
            access: RepositoryAccess::Admin,
        }];
        if read {
            members.push(RepositoryMember {
                subject: "bob-id".into(),
                name: "Bob".into(),
                access: RepositoryAccess::Read,
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

async fn fixture() -> (Harness, String, Uuid, Repo, Repo) {
    crate::test_support::server::init_test_tracing();
    let h = Harness::new(false).await;
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

    issue.bob_can_read(&h, true).await;
    pull.bob_can_read(&h, true).await;
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

    pull.bob_can_read(&h, false).await;
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
    pull.bob_can_read(&h, true).await;

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
    let moved_number = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let moved = h.json(&issue.api("/issues/1"), &alice).await;
            if moved["transfer"]["state"] == "ready" {
                break moved["transfer"]["number"].as_u64().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
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
    destination.bob_can_read(&h, true).await;
    issue.bob_can_read(&h, false).await;
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
