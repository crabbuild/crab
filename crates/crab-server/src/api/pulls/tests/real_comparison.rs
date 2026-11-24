use std::{collections::BTreeMap, path::Path, sync::Arc, time::Instant};

use reqwest::{Method, StatusCode, header};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    api::encode_hex,
    test_support::{
        git::{self, success as git_ok, with_http_token},
        http::{Harness, browser::Browser, rustfs_store},
    },
};

mod deep_history;

struct Case {
    name: &'static str,
    source: &'static str,
    merge: &'static str,
    paths: usize,
    review_file: &'static str,
    review_line: u32,
}

const CASES: [Case; 2] = [
    Case {
        name: "nextjs",
        source: "vercel/next.js.git",
        merge: "163830c026f61370b9c1f5d05005f52c91e7ecee",
        paths: 1179,
        review_file: "errors/no-document-title.md",
        review_line: 9,
    },
    Case {
        name: "neovim",
        source: "neovim/neovim",
        merge: "f907614088357d5e873e161daee5ac55ec9437bf",
        paths: 536,
        review_file: "test/benchmark/autocmd_spec.lua",
        review_line: 2,
    },
];

type Entry = (String, String);
type Changes = BTreeMap<String, [Option<Entry>; 2]>;

async fn git_changes(path: &Path, old: &str, new: &str) -> Changes {
    let args = [
        "diff",
        "--raw",
        "--no-abbrev",
        "--no-renames",
        "-z",
        old,
        new,
    ];
    let result = git::git(path, &args).await;
    assert!(result.status.success());
    if result.stdout.is_empty() {
        return Changes::new();
    }
    let records: Vec<_> = result
        .stdout
        .strip_suffix(&[0])
        .unwrap_or(&result.stdout)
        .split(|&byte| byte == 0)
        .collect();
    assert_eq!(records.len() % 2, 0);
    records
        .chunks_exact(2)
        .map(|record| {
            let metadata = std::str::from_utf8(record[0]).unwrap();
            let fields: Vec<_> = metadata
                .strip_prefix(':')
                .unwrap()
                .split_whitespace()
                .collect();
            assert_eq!(fields.len(), 5);
            let sides = [0, 1].map(|side| {
                (fields[side] != "000000")
                    .then(|| (fields[side].to_owned(), fields[side + 2].to_owned()))
            });
            (encode_hex(record[1]), sides)
        })
        .collect()
}

fn assert_changes(response: &Value, ancestor: &str, base: &str, head: &str, expected: &Changes) {
    assert_eq!(response["base"], ancestor);
    assert_eq!(response["base_tip"], base);
    assert_eq!(response["commit"], head);
    let rows = response["changes"].as_array().unwrap();
    assert_eq!(rows.len(), expected.len());
    let actual: Changes = rows
        .iter()
        .map(|row| {
            let sides = ["old", "new"].map(|side| {
                let entry = &row[side];
                (!entry.is_null()).then(|| {
                    (
                        entry["mode"].as_str().unwrap().to_owned(),
                        entry["oid"].as_str().unwrap().to_owned(),
                    )
                })
            });
            (row["path_hex"].as_str().unwrap().to_owned(), sides)
        })
        .collect();
    for (path, entries) in expected {
        assert_eq!(actual.get(path), Some(entries), "changed path {path}");
    }
}

async fn git_file(source: &Path, commit: &str, path: &str) -> String {
    let output = git::git(source, &["show", &format!("{commit}:{path}")]).await;
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

async fn request(h: &Harness, alice: &str, path: &str) -> (Value, u128) {
    let started = Instant::now();
    let value = h.json(path, alice).await;
    (value, started.elapsed().as_millis())
}

fn report(case: &Case, label: &str, mut samples: Vec<u128>) {
    samples.sort_unstable();
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    eprintln!(
        "review {} {label} samples={} p95_ms={p95}",
        case.name,
        samples.len(),
    );
}

async fn qualify(h: &Harness, alice: &str, case: &Case, root: &Path, scratch: &Path) {
    let source = root.join(case.source);
    let parents = git_ok(&source, &["show", "-s", "--format=%P", case.merge]).await;
    let tips: Vec<_> = parents.split_whitespace().collect();
    let [base, topic] = tips.as_slice() else {
        panic!("fixture must name a two-parent merge");
    };
    let ancestor = git_ok(&source, &["merge-base", "--all", base, topic]).await;
    assert_ne!(ancestor.as_str(), *base);
    assert_ne!(ancestor.as_str(), *topic);
    let expected = git_changes(&source, &ancestor, topic).await;
    assert_eq!(expected.len(), case.paths);
    let base_changes = git_changes(&source, &ancestor, base).await;
    assert!(base_changes.keys().any(|path| !expected.contains_key(path)));
    let old_file = git_file(&source, &ancestor, case.review_file).await;
    let new_file = git_file(&source, topic, case.review_file).await;
    assert_ne!(old_file, new_file);
    let path_hex = encode_hex(case.review_file.as_bytes());

    let local = scratch.join(case.name);
    git::bare_with_alternates(&source, &local).await;
    // Preserve the three real trees and their divergent relationship without
    // pushing unrelated history or writing objects to the input checkout.
    let common_tree = format!("{ancestor}^{{tree}}");
    let ancestor = git_ok(&local, &["commit-tree", &common_tree, "-m", "Common tree"]).await;
    let base = git_ok(
        &local,
        &[
            "commit-tree",
            &format!("{base}^{{tree}}"),
            "-p",
            &ancestor,
            "-m",
            "Base tree",
        ],
    )
    .await;
    let topic_tree = format!("{topic}^{{tree}}");
    let head = git_ok(
        &local,
        &[
            "commit-tree",
            &topic_tree,
            "-p",
            &ancestor,
            "-m",
            "Topic tree",
        ],
    )
    .await;
    git_ok(&local, &["update-ref", "refs/heads/main", &base]).await;
    git_ok(&local, &["update-ref", "refs/heads/feature", &head]).await;
    assert_eq!(git_changes(&local, &ancestor, &head).await, expected);

    h.add_repository(case.name).await;
    let credential: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            alice,
            json!({"owner":"team","repository":case.name,"access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = credential["token"].as_str().unwrap();
    let address = format!("/team/{}", case.name);
    let remote = format!("{}/git{address}.git", h.origin);
    let started = Instant::now();
    assert!(
        with_http_token(
            &local,
            token,
            &["push", "--atomic", &remote, "main", "feature"]
        )
        .await
    );
    eprintln!(
        "review {} push_ms={}",
        case.name,
        started.elapsed().as_millis()
    );

    let api = format!("/api/repos{address}");
    let compare =
        format!("{api}/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true");
    let diff = format!("{api}/diff?base={ancestor}&rev={head}&path_hex={path_hex}");
    let (cold, elapsed) = request(h, alice, &compare).await;
    assert_changes(&cold, &ancestor, &base, &head, &expected);
    eprintln!(
        "review {} paths={} cold_comparison_ms={elapsed}",
        case.name, case.paths
    );

    let mut comparisons = Vec::new();
    let mut files = Vec::new();
    for _ in 0..4 {
        let (a, b, c, d) = tokio::join!(
            request(h, alice, &compare),
            request(h, alice, &diff),
            request(h, alice, &compare),
            request(h, alice, &diff),
        );
        for (response, elapsed) in [a, c] {
            assert_changes(&response, &ancestor, &base, &head, &expected);
            comparisons.push(elapsed);
        }
        for (response, elapsed) in [b, d] {
            assert_eq!(response["old"]["text"], old_file);
            assert_eq!(response["new"]["text"], new_file);
            files.push(elapsed);
        }
    }
    report(case, "concurrent-comparison", comparisons);
    report(case, "concurrent-file", files);

    let browser = Browser::new(case.name);
    browser
        .run(&[
            "open",
            &format!("{}{address}/compare/main...feature", h.origin),
        ])
        .await;
    browser.sign_in().await;
    let files_changed = format!("{} changed files", case.paths);
    browser.run(&["wait", "--text", &files_changed]).await;
    let title = format!("Review the {} tree", case.name);
    let body = "Keep the discussion attached to the reviewed comparison.";
    browser.run(&["fill", "#discussion-title", &title]).await;
    browser
        .run(&["fill", "#discussion-description", body])
        .await;

    let updated = git_ok(
        &local,
        &[
            "commit-tree",
            &topic_tree,
            "-p",
            &head,
            "-m",
            "Advance topic",
        ],
    )
    .await;
    git_ok(
        &local,
        &["update-ref", "refs/heads/feature", &updated, &head],
    )
    .await;
    assert!(with_http_token(&local, token, &["push", &remote, "feature"]).await);
    let create = [
        "find",
        "role",
        "button",
        "click",
        "--name",
        "Create pull request",
        "--exact",
    ];
    browser.run(&create).await;
    browser
        .run(&["wait", "--text", "A branch moved or was deleted"])
        .await;
    let pull_url = format!("{api}/pulls/1");
    assert_eq!(
        h.http
            .get(format!("{}{pull_url}", h.origin))
            .header(header::COOKIE, alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Review inputs",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "form[aria-label='Create discussion'] button[type='submit']:not(:disabled)",
        ])
        .await;
    assert_eq!(
        browser
            .run(&["get", "value", "#discussion-title"])
            .await
            .trim(),
        title
    );
    assert_eq!(
        browser
            .run(&["get", "value", "#discussion-description"])
            .await
            .trim(),
        body
    );
    browser.run(&create).await;
    browser
        .run(&["wait", "--url", &format!("**{address}/pull/1")])
        .await;
    let pull = h.json(&pull_url, alice).await;
    assert_eq!(pull["base_oid"], base);
    assert_eq!(pull["original_head_oid"], updated);
    assert_eq!(pull["title"], title);
    assert_eq!(pull["body"], body);

    let threads = format!("{pull_url}/threads");
    let repo = h
        .server
        .repositories
        .get(&("team".into(), case.name.into()))
        .unwrap();
    let session = h.json("/api/session", alice).await;
    let response = h
        .http
        .post(format!("{}{threads}", h.origin))
        .header(header::COOKIE, alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repo.id.to_string())
        .json(&json!({
            "request_id": Uuid::now_v7(), "body": "Review this line in the corpus",
            "base_oid": ancestor, "head_oid": updated, "path_hex": path_hex,
            "side": "new", "start_line": case.review_line, "end_line": case.review_line,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{}",
        response.text().await.unwrap()
    );
    let thread = h.json(&format!("{threads}/1"), alice).await;
    assert_eq!(thread["base_oid"], ancestor);
    assert_eq!(thread["head_oid"], updated);
    assert_eq!(
        thread["new_blob_oid"],
        expected[&path_hex][1].as_ref().unwrap().1
    );
    assert_eq!(thread["current"], true);
    let viewed_path = format!("{pull_url}/viewed");
    let mut already_viewed = 0;
    let mut continuation = None;
    if case.paths > 950 {
        let put = h
            .http
            .put(format!("{}{viewed_path}", h.origin))
            .header(header::COOKIE, alice)
            .header(header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header("x-crab-repository-id", repo.id.to_string());
        let changes = h.json(&compare, alice).await;
        for file in changes["changes"].as_array().unwrap() {
            if file["path"] == case.review_file {
                continue;
            }
            let saved: Value = put
                .try_clone()
                .unwrap()
                .json(&json!({
                    "version": already_viewed, "viewed": true, "file": file["view_id"],
                }))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            already_viewed += 1;
            assert_eq!(saved["version"], already_viewed);
        }
        assert_eq!(already_viewed, case.paths - 1);
        let page = h.json(&viewed_path, alice).await;
        let after = page["next"].as_str().unwrap();
        continuation = Some(format!(
            "{viewed_path}?version={already_viewed}&after={after}"
        ));
    }
    let started = Instant::now();
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Files changed",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", &files_changed]).await;
    browser
        .run(&[
            "fill",
            "input[aria-label='Filter changed files']",
            case.review_file,
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            &format!("1 of {} files shown", case.paths),
        ])
        .await;
    browser.run(&["wait", "[data-thread-number='1']"]).await;
    eprintln!(
        "review {} browser_filtered_thread_ms={}",
        case.name,
        started.elapsed().as_millis()
    );
    let before = format!("{already_viewed} of {} files viewed", case.paths);
    browser.run(&["wait", "--text", &before]).await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            &format!("Viewed {}", case.review_file),
            "--exact",
        ])
        .await;
    let viewed = format!("{} of {} files viewed", already_viewed + 1, case.paths);
    browser.run(&["wait", "--text", &viewed]).await;
    if let Some(path) = continuation {
        let stale = h
            .http
            .get(format!("{}{path}", h.origin))
            .header(header::COOKIE, alice)
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        assert_eq!(
            stale.json::<Value>().await.unwrap()["error"]["code"],
            "page_changed"
        );
    }
    browser.run(&["reload"]).await;
    browser.run(&["wait", "--text", &viewed]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires pinned local Git inputs, loopback RustFS, and the embedded Chromium UI"]
async fn real_repository_pull_review_matches_git_and_reconfirms_a_moved_head() {
    let root = std::path::PathBuf::from(std::env::var("CRAB_REVIEW_CORPUS_DIR").unwrap());
    let local = tempfile::tempdir_in(std::env::var("CARGO_TARGET_DIR").unwrap()).unwrap();
    let h = Harness::new_with_store(false, false, rustfs_store("review-corpus")).await;
    h.initialize_directory().await;
    let maintenance = Arc::clone(&h.server.maintenance_admission);
    let slots = maintenance.available_permits() as u32;
    let permit = maintenance.acquire_many_owned(slots).await.unwrap();
    let alice = h.login().await;
    for case in &CASES {
        qualify(&h, &alice, case, &root, local.path()).await;
    }
    drop(permit);
    h.close().await;
}
