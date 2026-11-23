use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};

const ROOT: &str = "/api/repos/team/private";
const NAME: &str = "model.parquet";
const PATH: &str = "6d6f64656c2e70617271756574";
const ORIGINAL: &[u8] = b"\x00first stored file\n\xff";
const UPDATED: &[u8] = b"\x00second stored file\n\xff";

fn artifact(commit: &str) -> String {
    format!("{ROOT}/lfs-content?rev={commit}&path_hex={PATH}")
}

struct Fixture {
    h: Harness,
    source: tempfile::TempDir,
    token: String,
    alice: String,
}

impl Fixture {
    async fn new() -> Self {
        let h = Harness::new(false).await;
        let alice = h.login().await;
        h.write(
            Method::PUT,
            &format!("{ROOT}/settings/branch-protections"),
            &alice,
            json!({"expected_version":0,"rules":[]}),
        )
        .await
        .error_for_status()
        .unwrap();
        let token: Value = h
            .write(
                Method::POST,
                "/api/git-token",
                &alice,
                json!({"owner":"team","repository":"private","access":"write"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let source = tempfile::tempdir().unwrap();
        git::success(
            source.path(),
            &["init", "--object-format=sha1", "--initial-branch=main"],
        )
        .await;
        std::fs::write(source.path().join("README.md"), "A normal Git blob.").unwrap();
        Self {
            h,
            source,
            token: token["token"].as_str().unwrap().into(),
            alice,
        }
    }

    async fn push(&self, bytes: &[u8]) -> (String, Vec<u8>) {
        let digest = Sha256::digest(bytes);
        let oid = format!("{digest:x}");
        let pointer = crab_git::LfsPointer {
            oid: digest.into(),
            size: bytes.len() as u64,
            extensions: Vec::new(),
        }
        .serialize();
        let remote = format!("{}/git/team/private.git", self.h.origin);
        self.h
            .http
            .put(format!(
                "{remote}/info/lfs/objects/{oid}?size={}",
                bytes.len()
            ))
            .basic_auth("crab", Some(&self.token))
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let source = self.source.path();
        std::fs::write(source.join(NAME), &pointer).unwrap();
        git::success(source, &["add", "."]).await;
        git::success(source, &["commit", "-m", "Update LFS artifact"]).await;
        let commit = git::success(source, &["rev-parse", "HEAD"]).await;
        assert!(git::with_http_token(source, &self.token, &["push", &remote, "main"]).await);
        (commit, pointer)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lfs_content_streams_only_the_authenticated_pinned_file() {
    let f = Fixture::new().await;
    let (original, pointer) = f.push(ORIGINAL).await;
    let (current, _) = f.push(UPDATED).await;
    let h = &f.h;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let old_path = artifact(&original);
    let get = |path: &str| {
        h.http
            .get(format!("{}{path}", h.origin))
            .header(header::COOKIE, &bob)
    };
    let preview: Value = get(&format!(
        "{ROOT}/file-preview?rev={original}&path_hex={PATH}"
    ))
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(preview["lfs_size"], ORIGINAL.len());
    assert_eq!(preview["size"], pointer.len());

    let response = get(&old_path)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let etag = response.headers()[header::ETAG].clone();
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment"
    );
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(response.bytes().await.unwrap(), ORIGINAL);
    let range = get(&old_path)
        .header(header::RANGE, "bytes=2-6")
        .header(header::IF_RANGE, &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        range.headers()[header::CONTENT_RANGE],
        format!("bytes 2-6/{}", ORIGINAL.len())
    );
    assert_eq!(range.bytes().await.unwrap(), &ORIGINAL[2..7]);
    let outside = get(&old_path)
        .header(header::RANGE, "bytes=1000000-")
        .send()
        .await
        .unwrap();
    assert_eq!(outside.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    let full = get(&artifact(&current))
        .header(header::RANGE, "bytes=2-6")
        .header(header::IF_RANGE, &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(full.bytes().await.unwrap(), UPDATED);

    let raw = get(&format!("{ROOT}/blob?rev={original}&path_hex={PATH}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(raw.bytes().await.unwrap(), pointer);
    assert_eq!(
        get(&artifact("main")).send().await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let ordinary = format!("{ROOT}/lfs-content?rev={original}&path_hex=524541444d452e6d64");
    assert_eq!(
        get(&ordinary).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    let members = h.json(&format!("{ROOT}/members"), &f.alice).await;
    let alice = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["subject"] == "alice-id")
        .unwrap();
    h.write(Method::PUT, &format!("{ROOT}/members"), &f.alice,
        json!({"expected_revision":members["revision"],"members":[alice],"groups":members["groups"]}))
        .await.error_for_status().unwrap();
    assert_eq!(
        get(&old_path)
            .header(header::RANGE, "bytes=2-6")
            .header(header::IF_RANGE, &etag)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn browser_downloads_the_displayed_lfs_commit_after_the_branch_moves() {
    let f = Fixture::new().await;
    let (original, _) = f.push(ORIGINAL).await;
    let browser = Browser::new("lfs-content");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/blob/main/{NAME}", f.h.origin),
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
    let download = "a[aria-label='Download Git LFS file']";
    browser.run(&["wait", download]).await;
    browser.run(&["wait", "--text", "Raw pointer"]).await;
    let href = browser.run(&["get", "attr", download, "href"]).await;
    assert_eq!(href.trim(), artifact(&original));
    f.push(UPDATED).await;

    let destination = f.source.path().join("received.parquet");
    browser
        .run(&["download", download, destination.to_str().unwrap()])
        .await;
    assert_eq!(std::fs::read(destination).unwrap(), ORIGINAL);
    drop(browser);
    f.h.close().await;
}
