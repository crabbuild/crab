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
    source: tempfile::TempDir,
    remote: Url,
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
            source,
            remote,
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
        "partialFingerprints":{"primaryLocationLineHash":"stable-1"},
        "locations":[{"physicalLocation":{"artifactLocation":{"uri":"README.md"},
            "region":{"startLine":2}}}]}])
}

fn mixed_findings() -> Value {
    let mut mixed = finding();
    mixed[0]["locations"][0]["physicalLocation"]["region"]["startLine"] = json!(1);
    let mut introduced = finding()[0].clone();
    introduced["ruleId"] = json!("AUDIT-2");
    introduced["partialFingerprints"]["primaryLocationLineHash"] = json!("stable-2");
    mixed.as_array_mut().unwrap().push(introduced);
    mixed
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
    let mut changed_fingerprint = input.clone();
    changed_fingerprint["sarif"]["runs"][0]["results"][0]["partialFingerprints"]["primaryLocationLineHash"] =
        json!("changed");
    assert_eq!(f.upload(changed_fingerprint).await.0, StatusCode::CONFLICT);
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
async fn sarif_comparison_uses_the_latest_matching_scan_on_the_exact_base() {
    let f = Fixture::new().await;
    let pull = f.h.json(&format!("{ROOT}/1"), &f.alice).await;
    let base = pull["base_oid"].as_str().unwrap();
    let (status, head) = f.upload(report(&f.head, mixed_findings())).await;
    assert_eq!(status, StatusCode::CREATED, "{head}");
    let comparison = async |baseline: &str| {
        f.h.json(
            &format!(
                "/api/repos/team/private/commits/{}/check-runs/{}?baseline={baseline}",
                f.head, head["id"]
            ),
            &f.alice,
        )
        .await
    };
    assert_eq!(
        comparison(base).await["scan_comparison"]["state"],
        "missing"
    );
    assert_eq!(
        comparison(&"a".repeat(40)).await["scan_comparison"]["state"],
        "unreachable"
    );
    let mut unrelated = report(base, finding());
    unrelated["category"] = json!("other");
    assert_eq!(f.upload(unrelated).await.0, StatusCode::CREATED);
    assert_eq!(
        comparison(base).await["scan_comparison"]["state"],
        "missing"
    );
    let baseline_report = report(base, finding());
    let (status, archived) = f.upload(baseline_report.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let check = comparison(base).await;
    assert_eq!(
        check["scan_comparison"],
        json!({
            "baseline_sha":base,"state":"ready","introduced":1,"existing":1,"unclassified":0,
            "annotations":["existing","introduced"]
        })
    );
    assert_eq!(check["conclusion"], "failure");
    assert_eq!(f.merge().await, StatusCode::CONFLICT);
    let mut changed_tool = report(base, finding());
    changed_tool["sarif"]["runs"][0]["tool"]["driver"]["name"] = json!("Different scanner");
    assert_eq!(f.upload(changed_tool).await.0, StatusCode::CREATED);
    assert_eq!(
        comparison(base).await["scan_comparison"]["state"],
        "tools_changed"
    );
    let mut failed = report(base, json!([]));
    failed["sarif"]["runs"][0]["invocations"] = json!([{"executionSuccessful":false}]);
    assert_eq!(f.upload(failed).await.0, StatusCode::CREATED);
    assert_eq!(f.upload(baseline_report).await.1["id"], archived["id"]);
    assert_eq!(
        comparison(base).await["scan_comparison"]["state"],
        "incomplete"
    );
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the built embedded UI"]
async fn native_browser_shows_sarif_findings_and_opens_the_scanned_line() {
    let f = Fixture::new().await;
    let pull = f.h.json(&format!("{ROOT}/1"), &f.alice).await;
    let base = pull["base_oid"].as_str().unwrap();
    assert_eq!(
        f.upload(report(base, finding())).await.0,
        StatusCode::CREATED
    );
    let (status, result) = f.upload(report(&f.head, mixed_findings())).await;
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
        .run(&["wait", "--text", "1 new · 1 existing · 0 unclassified"])
        .await;
    browser.run(&["wait", "--fn", "Array.from(document.querySelectorAll('[aria-label=Annotations] li')).map(row => row.querySelector('span')?.textContent).join(',') === 'Existing,New'"]).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn scan_history_follows_branch_ancestry_and_rejects_a_moved_tip() {
    let f = Fixture::new().await;
    let pull = f.h.json(&format!("{ROOT}/1"), &f.alice).await;
    let base = pull["base_oid"].as_str().unwrap();
    assert_eq!(
        f.upload(report(base, finding())).await.0,
        StatusCode::CREATED
    );
    let first = f.upload(report(&f.head, finding())).await;
    assert_eq!(first.0, StatusCode::CREATED);
    let second = f.upload(report(&f.head, json!([]))).await;
    assert_eq!(second.0, StatusCode::CREATED);
    let (status, ordinary) = report_check(
        &f.h, &f.token, Method::POST, "/api/repos/team/private/check-runs",
        json!({
            "request_id":Uuid::now_v7(),"head_sha":f.head,"name":"Code scanning / indistinguishable title",
            "status":"completed","conclusion":"success","details_url":null,
            "output":{"title":"Code scanning results","summary":"An ordinary check run."}
        }),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "{ordinary}");

    let route = "/api/repos/team/private/code-scanning/history";
    let main = f.h.json(route, &f.alice).await;
    assert_eq!(main["branch"], "refs/heads/main");
    assert_eq!(main["tip"], base);
    assert_eq!(main["items"].as_array().unwrap().len(), 1);
    assert_eq!(main["items"][0]["oid"], base);
    let feature = format!("{route}?branch=refs%2Fheads%2Ffeature&limit=1");
    let page = f.h.json(&feature, &f.alice).await;
    assert_eq!(page["tip"], f.head);
    assert_eq!(page["tip_reports"], 2);
    assert_eq!(page["items"][0]["reports"], 2);
    let cursor = page["next"].as_str().unwrap();
    let continued = format!("{feature}&cursor={cursor}");
    let older = f.h.json(&continued, &f.alice).await;
    assert_eq!(older["tip_reports"], 2);
    assert_eq!(older["items"][0]["oid"], base);
    let status = async |url: &str, cookie: &str| {
        f.h.http
            .get(format!("{}{url}", f.h.origin))
            .header(header::COOKIE, cookie)
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(
        status(
            &format!("{route}?branch=refs%2Fheads%2Fmain&cursor={cursor}"),
            &f.alice
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    *f.h.provider.mode.lock().await = "outsider".into();
    let outsider = f.h.login().await;
    assert_eq!(status(route, &outsider).await, StatusCode::NOT_FOUND);

    let runs = format!("/api/repos/team/private/commits/{}/check-runs", f.head);
    let listed =
        f.h.json(&format!("{runs}?scans_only=true&limit=1"), &f.alice)
            .await;
    assert_eq!(listed["items"][0]["id"], second.1["id"]);
    let older_scan =
        f.h.json(
            &format!("{runs}?scans_only=true&limit=1&before={}", listed["next"]),
            &f.alice,
        )
        .await;
    assert_eq!(older_scan["items"][0]["id"], first.1["id"]);
    assert!(older_scan["next"].is_null());
    assert_eq!(
        status(
            &format!("{runs}/{}?scans_only=true", ordinary["id"]),
            &f.alice
        )
        .await,
        StatusCode::NOT_FOUND
    );

    git::success(
        f.source.path(),
        &["commit", "--allow-empty", "-m", "Unscanned branch tip"],
    )
    .await;
    git::success(
        f.source.path(),
        &["push", f.remote.as_str(), "HEAD:refs/heads/feature"],
    )
    .await;
    assert_eq!(status(&continued, &f.alice).await, StatusCode::CONFLICT);
    let moved = f.h.json(&feature, &f.alice).await;
    assert_eq!(moved["tip_reports"], 0);
    assert_eq!(moved["items"], json!([]));
    let older =
        f.h.json(
            &format!("{feature}&cursor={}", moved["next"].as_str().unwrap()),
            &f.alice,
        )
        .await;
    assert_eq!(older["items"][0]["oid"], f.head);
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the built embedded UI"]
async fn native_browser_opens_scan_history_from_repository_security() {
    let f = Fixture::new().await;
    let pull = f.h.json(&format!("{ROOT}/1"), &f.alice).await;
    let base = pull["base_oid"].as_str().unwrap();
    let mut on_main = report(base, finding());
    on_main["category"] = json!("main audit");
    assert_eq!(f.upload(on_main).await.0, StatusCode::CREATED);
    let mut on_feature = report(&f.head, mixed_findings());
    on_feature["category"] = json!("feature audit");
    assert_eq!(f.upload(on_feature).await.0, StatusCode::CREATED);

    let browser = Browser::new("security-history");
    browser
        .run(&["open", &format!("{}/team/private", f.h.origin)])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Security"]).await;
    browser
        .run(&[
            "find", "role", "link", "click", "--name", "Security", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Code scanning / main audit"])
        .await;
    browser
        .choose_select("#scan-branch", "refs/heads/feature")
        .await;
    browser
        .run(&["wait", "--text", "Code scanning / feature audit"])
        .await;
    browser.run(&["wait", "--text", "Error · AUDIT-2"]).await;
    let older = format!("[aria-label='Scanned commits'] a[href*='scan_commit={base}']");
    browser.run(&["wait", &older]).await;
    browser.run(&["click", &older]).await;
    browser
        .run(&["wait", "--text", "Code scanning / main audit"])
        .await;
    let location = format!(
        "document.querySelector('[aria-label=Annotations] a')?.getAttribute('href')?.includes('/blob/{base}/README.md')"
    );
    browser.run(&["wait", "--fn", &location]).await;
    f.h.close().await;
}
