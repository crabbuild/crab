use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use crab_storage::{StorageObservation, StorageObserver, StorageOperation};

use super::*;

const CORPUS: [(&str, &str, &str); 5] = [
    (
        "nextjs",
        "vercel/next.js.git",
        "d8c1c1b0002db8ccb63b58095f338e8bf423fa99",
    ),
    (
        "neovim",
        "neovim/neovim",
        "64827e1ab9279b14044400b975b040568120c895",
    ),
    (
        "ripgrep",
        "BurntSushi/ripgrep",
        "3fce3b5bb0236da2df6d99672afb8a719642eca7",
    ),
    (
        "tanstack-query",
        "TanStack/query",
        "ee47993724e5b71e65d9f38ae416367d2dd18395",
    ),
    (
        "zod",
        "colinhacks/zod",
        "d2b135cfb7a3582b9eb515756b9166bcb9521f4a",
    ),
];

#[derive(Default)]
struct StorageReads(AtomicU64);

impl StorageObserver for StorageReads {
    fn started(&self, _: StorageOperation) {}

    fn finished(&self, observation: StorageObservation) {
        if matches!(
            observation.operation,
            StorageOperation::Get
                | StorageOperation::Head
                | StorageOperation::Range
                | StorageOperation::List
        ) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl StorageReads {
    fn total(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

async fn publish(
    h: &Harness,
    account: &str,
    local: &Path,
    input: &Path,
    name: &str,
    pin: &str,
) -> String {
    let tree = git::success(input, &["rev-parse", &format!("{pin}^{{tree}}")]).await;
    let path = local.join(name);
    // The new root uses the source tree with no writes to the input repository.
    git::bare_with_alternates(input, &path).await;
    let head = git::success(&path, &["commit-tree", &tree, "-m", pin]).await;
    git::success(&path, &["update-ref", "refs/heads/main", &head]).await;

    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            account,
            json!({"owner":"team", "repository":name, "access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let remote = format!("{}/git/team/{name}.git", h.origin);
    let started = Instant::now();
    assert!(
        git::with_http_token(
            &path,
            token["token"].as_str().unwrap(),
            &["push", &remote, "main"]
        )
        .await,
        "could not publish the pinned {name} tree"
    );
    eprintln!(
        "corpus {name} {pin} tree={tree} push_ms={}",
        started.elapsed().as_millis()
    );
    head
}

fn endpoint(query: &str) -> String {
    format!(
        "/api/code-search?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", query)
            .append_pair("case_sensitive", "true")
            .finish()
    )
}

#[derive(Default)]
struct Evidence {
    locations: BTreeMap<(String, String), Option<u64>>,
    first_cursor: Option<String>,
    scanned: u64,
    verified: u64,
    scanning: u64,
    durations_ms: Vec<u128>,
}

async fn query(h: &Harness, account: &str, url: &str, heads: &BTreeMap<&str, String>) -> Evidence {
    let mut proof = Evidence::default();
    let mut path = url.to_owned();
    loop {
        assert!(
            proof.durations_ms.len() < 100,
            "search did not exhaust its continuation"
        );
        let started = Instant::now();
        let response = h.json(&path, account).await;
        proof.durations_ms.push(started.elapsed().as_millis());
        assert_eq!(response["limited_repositories"], 0);
        proof.scanning += response["scanning_repositories"].as_u64().unwrap();
        proof.scanned += response["scanned"].as_u64().unwrap();
        proof.verified += response["verified"].as_u64().unwrap();
        for group in response["groups"].as_array().unwrap() {
            let name = group["name"].as_str().unwrap();
            assert_eq!(group["owner"], "team");
            assert_eq!(group["commit"], heads[name]);
            for file in group["files"].as_array().unwrap() {
                let path = file["path"].as_str().unwrap();
                let line = file["lines"][0]["line"].as_u64();
                assert!(
                    proof
                        .locations
                        .insert((name.to_owned(), path.to_owned()), line)
                        .is_none()
                );
            }
        }
        let Some(cursor) = response["next"].as_str() else {
            break;
        };
        proof.first_cursor.get_or_insert_with(|| cursor.to_owned());
        path = format!("{url}&cursor={cursor}");
    }
    proof
}

fn latency(label: &str, mut pages: Vec<u128>) {
    pages.sort_unstable();
    let percentile = |rank: usize| pages[(rank * pages.len()).div_ceil(100) - 1];
    eprintln!(
        "search {label} pages={} p50_ms={} p95_ms={} max_ms={}",
        pages.len(),
        percentile(50),
        percentile(95),
        pages.last().unwrap(),
    );
}

async fn warm_query(
    h: &Harness,
    account: &str,
    label: &str,
    url: &str,
    heads: &BTreeMap<&str, String>,
) -> Evidence {
    let first = query(h, account, url, heads).await;
    assert_eq!(first.scanning, 0, "{label}");
    let mut samples = first.durations_ms.clone();
    for _ in 0..2 {
        let repeat = query(h, account, url, heads).await;
        assert_eq!(repeat.scanning, 0, "{label}");
        assert_eq!(repeat.locations, first.locations, "{label}");
        samples.extend(repeat.durations_ms);
    }
    latency(label, samples);
    eprintln!(
        "search {label} scanned={} verified={} hits={}",
        first.scanned,
        first.verified,
        first.locations.len(),
    );
    first
}

fn locations(items: &[(&str, &str, Option<u64>)]) -> BTreeMap<(String, String), Option<u64>> {
    items
        .iter()
        .map(|(name, path, line)| ((name.to_string(), path.to_string()), *line))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires pinned local inputs and an isolated RustFS bucket"]
async fn workspace_search_indexes_and_checks_a_real_repository_corpus() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let root = std::path::PathBuf::from(required("CRAB_SEARCH_CORPUS_DIR"));
    let scratch = std::path::PathBuf::from(required("CARGO_TARGET_DIR"));
    let local = tempfile::tempdir_in(scratch).unwrap();
    let reads = Arc::new(StorageReads::default());
    let store = crate::test_support::http::rustfs_store("search-corpus")
        .with_storage_observer(reads.clone());
    let h = Harness::new_with_store(false, false, store).await;
    // Exercise the search index without starting unrelated browse projection
    // builds, which compete with the measurement and have smaller fixture limits.
    let maintenance = &h.server.maintenance_admission;
    let permit = Arc::clone(maintenance)
        .acquire_many_owned(u32::try_from(maintenance.available_permits()).unwrap())
        .await
        .unwrap();
    let alice = h.login().await;
    let mut heads = BTreeMap::new();
    for (name, source, pin) in CORPUS {
        h.add_repository(name).await;
        let head = publish(&h, &alice, local.path(), &root.join(source), name, pin).await;
        heads.insert(name, head);
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;

    let next_path = "packages/next/src/shared/lib/router/utils/resolve-rewrites.ts";
    let next = ("nextjs", next_path, Some(12));
    let nvim = ("neovim", "src/nvim/api/vim.c", Some(714));
    let rg = ("ripgrep", "crates/core/main.rs", Some(78));
    let directory = "packages/next/src/shared/lib/router/utils";
    let path = format!("path:{next_path}");
    let literal = r#"content:"export default function resolveRewrites(""#;
    let regex = r"content:/export default function resolveRewrit(?:es)\(/";
    let symbol = "symbol:resolveRewrites";
    let boolean = endpoint(
        r#"symbol:resolveRewrites OR symbol:nvim_set_current_line OR content:"fn run(result: crate::flags::ParseResult<HiArgs>""#,
    );
    let scoped = |term: &str| {
        let route = endpoint(&format!("repo:team/nextjs {term}"));
        format!("{route}&directory={directory}")
    };
    let initial_reads = reads.total();
    let cold = query(&h, &bob, &scoped(symbol), &heads).await;
    assert_eq!(cold.locations, locations(&[next]));
    assert!(
        cold.scanning > 0,
        "expected a scan before building the index"
    );
    latency("before-index", cold.durations_ms);
    eprintln!(
        "search before-index storage_reads={}",
        reads.total() - initial_reads
    );

    let during_reads = reads.total();
    let ((), during) = tokio::join!(
        async {
            for name in heads.keys() {
                let repo = h
                    .server
                    .repositories
                    .get(&("team".into(), (*name).into()))
                    .unwrap();
                let started = Instant::now();
                code_index::reconcile(&h.server, &repo, &h.server.cancellation)
                    .await
                    .unwrap();
                eprintln!("corpus {name} build_ms={}", started.elapsed().as_millis());
            }
        },
        async {
            let path = scoped(&path);
            let literal = scoped(literal);
            let regex = scoped(regex);
            let symbol = scoped(symbol);
            tokio::join!(
                query(&h, &bob, &path, &heads),
                query(&h, &bob, &literal, &heads),
                query(&h, &bob, &regex, &heads),
                query(&h, &bob, &symbol, &heads),
            )
        }
    );
    assert_eq!(
        during.0.locations,
        locations(&[("nextjs", next_path, None)])
    );
    let mut build_samples = during.0.durations_ms;
    for result in [during.1, during.2, during.3] {
        assert_eq!(result.locations, locations(&[next]));
        build_samples.extend(result.durations_ms);
    }
    latency("during-index-build", build_samples);
    eprintln!(
        "search and build storage_reads={}",
        reads.total() - during_reads
    );

    let path = endpoint(&path);
    let literal = endpoint(literal);
    let regex = endpoint(regex);
    let warm_reads = reads.total();
    let (by_path, by_literal, by_regex, before) = tokio::join!(
        warm_query(&h, &bob, "path", &path, &heads),
        warm_query(&h, &bob, "literal", &literal, &heads),
        warm_query(&h, &bob, "regex", &regex, &heads),
        warm_query(&h, &bob, "boolean+symbols", &boolean, &heads),
    );
    eprintln!(
        "search warm-mixed storage_reads={}",
        reads.total() - warm_reads
    );
    assert_eq!(by_path.locations, locations(&[("nextjs", next_path, None)]));
    assert_eq!(by_path.verified, 0);
    for result in [by_literal, by_regex] {
        assert_eq!(result.locations, locations(&[next]));
        assert!(
            result.verified < 160,
            "the selective query verified too many blobs"
        );
    }
    assert_eq!(before.locations, locations(&[next, nvim, rg]));
    assert!(before.scanned >= 38_000);
    assert!(before.verified < 256);
    let cursor = format!("{boolean}&cursor={}", before.first_cursor.unwrap());

    let browser = crate::test_support::http::browser::Browser::new("search-corpus");
    browser
        .run(&["open", &format!("{}/-/code-search", h.origin)])
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
    browser.run(&["wait", "#workspace-search-text"]).await;
    browser
        .run(&[
            "fill",
            "#workspace-search-directory",
            "packages/next/src/shared/lib/router/utils",
        ])
        .await;
    browser
        .run(&[
            "fill",
            "#workspace-search-text",
            &format!("repo:team/nextjs path:{next_path} symbol:resolveRewrites"),
        ])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    let head = &heads["nextjs"];
    let link = format!("a[href*='/blob/{head}/{next_path}'][href$='#L12']");
    browser.run(&["wait", &link]).await;
    browser.run(&["reload"]).await;
    browser.run(&["wait", &link]).await;
    browser.run(&["click", &link]).await;
    browser
        .run(&[
            "wait",
            "--url",
            &format!("**/blob/{head}/{next_path}**#L12"),
        ])
        .await;
    drop(browser);

    let membership = "/api/repos/team/nextjs/members";
    let current = h.json(membership, &alice).await;
    let mut members = current["members"].as_array().unwrap().clone();
    members.retain(|member| member["subject"] != "bob-id");
    let session = h.json("/api/session", &alice).await;
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "nextjs".into()))
        .unwrap();
    let changed = h
        .http
        .put(format!("{}{membership}", h.origin))
        .header(reqwest::header::COOKIE, &alice)
        .header(reqwest::header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repo.id.to_string())
        .json(&json!({
            "expected_revision":current["revision"], "groups":current["groups"], "members":members,
        }))
        .send()
        .await
        .unwrap();
    let status = changed.status();
    assert_eq!(status, StatusCode::OK, "{}", changed.text().await.unwrap());
    let denied = h
        .http
        .get(format!("{}{cursor}", h.origin))
        .header(reqwest::header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::CONFLICT);
    let after = query(&h, &bob, &boolean, &heads).await;
    assert_eq!(after.scanning, 0);
    assert_eq!(after.locations, locations(&[nvim, rg]));
    h.close().await;
    drop(permit);
}
