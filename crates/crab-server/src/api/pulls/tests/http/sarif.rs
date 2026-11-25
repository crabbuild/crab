use crate::test_support::{git, http::browser::Browser};
use reqwest::Method;
use uuid::Uuid;

use super::*;

const UPLOAD: &str = "/api/repos/team/private/check-runs/sarif";
const CHECK: &str = "Code scanning / audit";

struct Fixture {
    h: Harness,
    alice: String,
    token: String,
    head: String,
}

impl Fixture {
    async fn new() -> Self {
        let h = Harness::new(false).await;
        h.initialize_directory().await;
        let alice = h.login().await;
        let token = token(&h, &alice, "write").await;
        let source = tempfile::tempdir().unwrap();
        let mut remote = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
        remote.set_username("crab").unwrap();
        remote.set_password(Some(&token)).unwrap();
        let head = git::push_review_branches(source.path(), remote.as_str()).await;
        h.write(
            Method::POST,
            ROOT,
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Scanner report","body":"",
                "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
        )
        .await
        .error_for_status()
        .unwrap();
        h.write(
            Method::PUT,
            "/api/repos/team/private/settings/branch-protections",
            &alice,
            json!({"expected_version":0,"rules":[{
                "branch":"main","required_approvals":0,"required_checks":[CHECK]
            }]}),
        )
        .await
        .error_for_status()
        .unwrap();
        Self {
            h,
            alice,
            token,
            head,
        }
    }

    async fn upload(&self, report: Value) -> (StatusCode, Value) {
        report_check(&self.h, &self.token, Method::POST, UPLOAD, report).await
    }

    async fn merge(&self) -> StatusCode {
        let pull = self.h.json(&format!("{ROOT}/1"), &self.alice).await;
        self.h
            .write(
                Method::POST,
                &format!("{ROOT}/1/merge"),
                &self.alice,
                json!({"request_id":Uuid::now_v7(),"version":pull["version"],
                    "method":"merge_commit","base_oid":pull["base_oid"],"head_oid":pull["head_oid"]}),
            )
            .await
            .status()
    }
}

async fn token(h: &Harness, actor: &str, access: &str) -> String {
    let response: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            actor,
            json!({"owner":"team","repository":"private","access":access}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    response["token"].as_str().unwrap().into()
}

fn report(oid: &str, results: Value) -> Value {
    json!({
        "request_id":Uuid::now_v7(),"head_sha":oid,"category":"audit",
        "sarif":{"version":"2.1.0","runs":[{
            "tool":{"driver":{"name":"Example scanner"}},
            "results":results,
        }]}
    })
}

fn finding() -> Value {
    json!([{"ruleId":"AUDIT-1","level":"error","message":{"text":"Review this example finding."},
        "locations":[{"physicalLocation":{"artifactLocation":{"uri":"README.md"},
            "region":{"startLine":2}}}]}])
}

#[tokio::test(flavor = "multi_thread")]
async fn sarif_uses_exact_commit_authority_and_gates_required_merges() {
    let f = Fixture::new().await;
    let input = report(&f.head, finding());
    let reader = token(&f.h, &f.alice, "read").await;
    assert_eq!(
        report_check(&f.h, &reader, Method::POST, UPLOAD, input.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let invalid = report(&"a".repeat(40), json!([]));
    assert_eq!(f.upload(invalid).await.0, StatusCode::NOT_FOUND);
    let (status, failure) = f.upload(input.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{failure}");
    assert_eq!(failure["head_sha"], f.head);
    assert_eq!(failure["conclusion"], "failure");
    assert_eq!(failure["name"], CHECK);
    assert_eq!(f.upload(input.clone()).await.1, failure);
    let mut collision = input;
    collision["sarif"]["runs"][0]["results"] = json!([]);
    assert_eq!(f.upload(collision).await.0, StatusCode::CONFLICT);
    let pull = f.h.json(&format!("{ROOT}/1"), &f.alice).await;
    assert_eq!(
        pull["merge_requirements"]["checks"][0]["run_id"],
        failure["id"]
    );
    assert_eq!(f.merge().await, StatusCode::CONFLICT);
    let older = report(pull["base_oid"].as_str().unwrap(), json!([]));
    assert_eq!(f.upload(older).await.0, StatusCode::CREATED);
    assert_eq!(f.merge().await, StatusCode::CONFLICT);
    let (status, clean) = f.upload(report(&f.head, json!([]))).await;
    assert_eq!(status, StatusCode::CREATED, "{clean}");
    assert_eq!(clean["conclusion"], "success");
    assert_eq!(f.merge().await, StatusCode::OK);
    assert_eq!(
        f.h.json(&format!("{ROOT}/1"), &f.alice).await["state"],
        "merged"
    );
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the built embedded UI"]
async fn native_browser_shows_sarif_findings_and_opens_the_scanned_line() {
    let f = Fixture::new().await;
    let (status, result) = f.upload(report(&f.head, finding())).await;
    assert_eq!(status, StatusCode::CREATED, "{result}");
    let browser = Browser::new("sarif");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/pull/1/checks", f.h.origin),
        ])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Error · AUDIT-1"]).await;
    browser
        .run(&["wait", "--text", "Review this example finding."])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "README.md:2",
            "--exact",
        ])
        .await;
    let pinned = format!("/team/private/blob/{}/README.md", f.head);
    let location = format!(
        "location.pathname === {} && location.hash === '#L2'",
        json!(pinned)
    );
    browser.run(&["wait", "--fn", &location]).await;
    f.h.close().await;
}
