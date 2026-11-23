use super::*;
use crate::test_support::{git, http::Harness};
use reqwest::Method;
use serde_json::{Value, json};

#[tokio::test]
async fn code_search_pins_its_pages_and_requires_current_repository_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
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
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    let secret = token["token"].as_str().unwrap();
    remote.set_password(Some(secret)).unwrap();
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    git::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::create_dir_all(path.join("src/nested")).unwrap();
    std::fs::write(
        path.join("src/nested/a.rs"),
        b"heading\r\nlet x = \"Caf\xc3\xa9.*\";\r\n",
    )
    .unwrap();
    std::fs::write(path.join("src/z.rs"), "Café.* follows here\n").unwrap();
    std::fs::write(path.join("outside.rs"), "Café.* outside the scope\n").unwrap();
    std::fs::write(path.join("src/binary"), b"\0Caf\xc3\xa9.*").unwrap();
    std::fs::write(path.join("src/large"), "Café.*".repeat(50_000)).unwrap();
    git::success(path, &["add", "."]).await;
    git::success(path, &["commit", "-m", "Searchable commit"]).await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["branch", "topic"]).await;
    for branch in ["main", "topic"] {
        let pushed = git::git(path, &["push", remote.as_str(), branch]).await;
        assert!(
            pushed.status.success(),
            "{}",
            String::from_utf8_lossy(&pushed.stderr).replace(secret, "[redacted]")
        );
    }
    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    let resource = "/api/repos/team/private/code-search";
    let first = format!("{resource}?rev=refs%2Fheads%2Ftopic&path=src&q=CAF%C3%89.*&limit=1");
    let result = h.json(&first, &reader).await;
    assert_eq!(result["commit"], head);
    assert_eq!(result["skipped"], 2);
    assert_eq!(result["items"][0]["path"], "src/nested/a.rs");
    assert_eq!(result["items"][0]["lines"][0]["line"], 2);
    assert_eq!(result["items"][0]["lines"][0]["matched"], "Café.*");
    let cursor = result["next"].as_str().unwrap();
    let mismatched = format!("{first}&case_sensitive=true&cursor={cursor}");
    let status = |url: String| async {
        h.http
            .get(url)
            .header(reqwest::header::COOKIE, &reader)
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(
        status(format!("{}{mismatched}", h.origin)).await,
        StatusCode::BAD_REQUEST
    );

    git::success(path, &["checkout", "topic"]).await;
    std::fs::write(path.join("src/z.rs"), "Changed on the topic branch\n").unwrap();
    git::success(path, &["commit", "-am", "Advance topic"]).await;
    let pushed = git::git(path, &["push", remote.as_str(), "topic"]).await;
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr).replace(secret, "[redacted]")
    );
    let continuation = format!("{first}&cursor={cursor}");
    let second = h.json(&continuation, &reader).await;
    assert_eq!(second["commit"], head);
    assert_eq!(second["items"][0]["path"], "src/z.rs");
    assert_eq!(second["next"], Value::Null);
    let fresh = h.json(&first, &reader).await;
    assert_ne!(fresh["commit"], head);
    let sensitive = h
        .json(&format!("{first}&case_sensitive=true"), &reader)
        .await;
    assert_eq!(sensitive["items"], json!([]));

    let catalog = h.server.catalog().unwrap();
    let mut access = catalog.membership("team", "private").await.unwrap().members;
    access.retain(|member| member.subject != "bob-id");
    catalog
        .set_members("team", "private", access, true)
        .await
        .unwrap();
    assert_eq!(
        status(format!("{}{continuation}", h.origin)).await,
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

async fn searchable_repository(
    h: &Harness,
    account: &str,
    owner: &str,
    name: &str,
    files: impl IntoIterator<Item = (String, String)>,
) -> (tempfile::TempDir, url::Url, String) {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            account,
            json!({"owner":owner,"repository":name,"access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/{owner}/{name}.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    git::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    for (filename, data) in files {
        let file = path.join(filename);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, data).unwrap();
    }
    git::success(path, &["add", "."]).await;
    git::success(path, &["commit", "-m", "Make code searchable"]).await;
    push_workspace_fixture(path, &remote).await;
    let oid = git::success(path, &["rev-parse", "HEAD"]).await;
    (source, remote, oid)
}

async fn push_workspace_fixture(path: &std::path::Path, remote: &url::Url) {
    let result = git::git(path, &["push", remote.as_str(), "main"]).await;
    assert!(
        result.status.success(),
        "Fixture push failed: {}",
        String::from_utf8_lossy(&result.stderr).replace(remote.password().unwrap(), "[redacted]")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_search_pins_current_repository_and_invalidates_pages_after_revocation() {
    let h = Harness::new(false).await;
    for name in ["library", "restricted", "empty"] {
        h.add_repository(name).await;
    }
    let alice = h.login().await;
    let (source, remote, head) = searchable_repository(
        &h,
        &alice,
        "team",
        "private",
        (0..24)
            .map(|index| (format!("src/file-{index:02}.rs"), "fn Café() {}\n".into()))
            .chain((0..12).map(|index| {
                (
                    format!("src/neutral-{index:02}.rs"),
                    "fn neutral() { Café(); }\n".into(),
                )
            }))
            .chain([("config/manifest.toml".into(), "limited path match\n".into())]),
    )
    .await;
    let (_, _, library_oid) = searchable_repository(
        &h,
        &alice,
        "team",
        "library",
        [
            ("src/library.rs".into(), "fn Café() {}\n".into()),
            ("config/manifest.yml".into(), String::new()),
        ],
    )
    .await;
    searchable_repository(
        &h,
        &alice,
        "team",
        "restricted",
        [
            (
                "src/restricted.rs".into(),
                "fn Café() { /* PRIVATE_ONLY */ }\n".into(),
            ),
            ("config/manifest.txt".into(), String::new()),
        ],
    )
    .await;
    let remove_reader = async |repo: &str| {
        let catalog = h.server.catalog().unwrap();
        let mut access = catalog.membership("team", repo).await.unwrap().members;
        access.retain(|member| member.subject != "bob-id");
        catalog
            .set_members("team", repo, access, true)
            .await
            .unwrap();
    };
    remove_reader("restricted").await;
    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    let path = "/api/code-search?q=MANIFEST&kind=path&directory=config";
    let all = h.json(path, &reader).await;
    let hits: Vec<_> = all["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (&g["name"], &g["files"][0]["path"]))
        .collect();
    assert_eq!(
        hits,
        [
            (&json!("private"), &json!("config/manifest.toml")),
            (&json!("library"), &json!("config/manifest.yml")),
        ]
    );
    assert_eq!(all["groups"][1]["commit"], library_oid);
    assert_eq!(all["groups"][0]["files"][0]["lines"], json!([]));
    assert_eq!(all["next"], Value::Null);

    let no_match = "/api/code-search?q=absent&directory=src&kind=path";
    let empty = h.json(no_match, &reader).await;
    assert_eq!(empty["groups"], json!([]));
    assert_eq!(empty["scanned"], 32);
    let more = empty["next"].as_str().unwrap();
    let exhausted = h.json(&format!("{no_match}&cursor={more}"), &reader).await;
    assert_eq!(exhausted["groups"], json!([]));
    assert_eq!(exhausted["next"], Value::Null);

    let query = "/api/code-search?q=CAF%C3%89&directory=src&kind=symbol";
    let first = h.json(query, &reader).await;
    assert_eq!(first["groups"][0]["commit"], head);
    assert_eq!(first["groups"][0]["files"].as_array().unwrap().len(), 20);
    let cursor = first["next"].as_str().unwrap();
    let cursor_request = format!("{query}&cursor={cursor}");
    let status = async |url: &str, cookie: &str| {
        h.http
            .get(format!("{}{url}", h.origin))
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(
        status(&cursor_request, &alice).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status(&format!("{cursor_request}&case_sensitive=true"), &reader).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status(
            &cursor_request.replace("kind=symbol", "kind=content"),
            &reader
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let protections = "/api/repos/team/private/settings/branch-protections";
    let catalog = h.json("/api/repos?owner=team&name=private", &alice).await;
    let version = &catalog["repositories"][0]["protection_version"];
    h.write(
        Method::PUT,
        protections,
        &alice,
        json!({"expected_version":version,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    std::fs::write(source.path().join("src/file-23.rs"), "let new_value = 1;\n").unwrap();
    git::success(source.path(), &["commit", "-am", "Advance default branch"]).await;
    push_workspace_fixture(source.path(), &remote).await;
    let second = h.json(&cursor_request, &reader).await;
    assert_eq!(second["groups"][0]["commit"], head);
    assert_eq!(second["groups"][0]["files"][3]["path"], "src/file-23.rs");
    assert_eq!(
        second["groups"][0]["files"][3]["lines"][0]["matched"],
        "Café"
    );
    assert_eq!(second["groups"][1]["commit"], library_oid);
    assert_eq!(second["next"], Value::Null);

    remove_reader("private").await;
    assert_eq!(status(&cursor_request, &reader).await, StatusCode::CONFLICT);
    let fresh = h.json(query, &reader).await;
    assert_eq!(fresh["groups"].as_array().unwrap().len(), 1);
    assert_eq!(fresh["groups"][0]["name"], "library");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_search_organization_scope_follows_the_current_catalog() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "workspace-search").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let created: Value = h
        .write(
            Method::POST,
            &format!("/api/organizations/{organization}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = created["owner"].as_str().unwrap();
    for (namespace, name) in [("team", "private"), (owner, "library")] {
        searchable_repository(
            &h,
            &alice,
            namespace,
            name,
            [("settings.txt".into(), "release gates\n".into())],
        )
        .await;
    }
    let root = "/api/code-search?q=release+gates";
    assert_eq!(
        h.json(root, &alice).await["groups"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let result = h
        .json(&format!("{root}&organization={organization}"), &alice)
        .await;
    assert_eq!(result["groups"].as_array().unwrap().len(), 1);
    assert_eq!(result["groups"][0]["id"], created["id"]);
    for scope in [owner.to_owned(), organization.to_string()] {
        let result = h.json(&format!("{root}+org%3A{scope}"), &alice).await;
        assert_eq!(result["groups"].as_array().unwrap().len(), 1);
        assert_eq!(result["groups"][0]["id"], created["id"]);
    }
    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    let result = h
        .json(&format!("{root}&organization={organization}"), &reader)
        .await;
    assert_eq!(result["groups"], json!([]));
    assert_eq!(
        h.json(&format!("{root}+org%3A{owner}"), &reader).await["groups"],
        json!([])
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn code_search_qualifiers_filter_pinned_results_without_widening_access() {
    let h = Harness::new(false).await;
    h.add_repository("restricted").await;
    let alice = h.login().await;
    let (_, _, head) = searchable_repository(
        &h,
        &alice,
        "team",
        "private",
        [
            ("src/direct.rs".into(), "fn qualified_needle() {}\n".into()),
            (
                "src/nested/code.rs".into(),
                "fn qualified_needle() {}\n".into(),
            ),
            (
                "src/elsewhere.py".into(),
                "def qualified_needle(): pass\n".into(),
            ),
            (
                "docs/src/example.rs".into(),
                "fn qualified_needle() {}\n".into(),
            ),
            (
                "docs/release notes.md".into(),
                "repo:team/restricted\n".into(),
            ),
        ],
    )
    .await;
    searchable_repository(
        &h,
        &alice,
        "team",
        "restricted",
        [("src/hidden.rs".into(), "fn qualified_needle() {}\n".into())],
    )
    .await;
    let catalog = h.server.catalog().unwrap();
    let mut members = catalog
        .membership("team", "restricted")
        .await
        .unwrap()
        .members;
    members.retain(|member| member.subject != "bob-id");
    catalog
        .set_members("team", "restricted", members, true)
        .await
        .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    let qualified =
        "q=repo%3ATEAM%2Fprivate+language%3ARust+path%3A%2Fsrc%2F**%2F*+symbol%3Aqualified_needle";
    let private = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    for indexed in [false, true] {
        if indexed {
            code_index::reconcile(&h.server, &private, &h.server.cancellation)
                .await
                .unwrap();
        }
        let result = h
            .json(&format!("/api/code-search?{qualified}"), &reader)
            .await;
        assert_eq!(result["indexed_repositories"], usize::from(indexed));
        assert_eq!(result["groups"].as_array().unwrap().len(), 1);
        let group = &result["groups"][0];
        assert_eq!(group["commit"], head);
        let files = group["files"].as_array().unwrap();
        assert_eq!(
            files
                .iter()
                .map(|hit| hit["path"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["src/direct.rs", "src/nested/code.rs"]
        );
        assert_eq!(files[0]["lines"][0]["matched"], "qualified_needle");
    }

    let hidden = h
        .json(
            "/api/code-search?q=repo%3Ateam%2Frestricted+symbol%3Aqualified_needle",
            &reader,
        )
        .await;
    let absent = h
        .json(
            "/api/code-search?q=repo%3Ateam%2Fmissing+symbol%3Aqualified_needle",
            &reader,
        )
        .await;
    assert_eq!(hidden, absent);
    assert_eq!(hidden["groups"], json!([]));

    let query = "/api/repos/team/private/code-search?q=path%3A%22release+notes%22+content%3A%22repo%3Ateam%2Frestricted%22";
    let literal = h.json(query, &reader).await;
    assert_eq!(literal["items"][0]["path"], "docs/release notes.md");
    assert_eq!(
        literal["items"][0]["lines"][0]["matched"],
        "repo:team/restricted"
    );

    let endpoint = "/api/repos/team/private/code-search?limit=1";
    let first = h.json(&format!("{endpoint}&{qualified}"), &reader).await;
    let cursor = first["next"].as_str().unwrap();
    let response = h
        .http
        .get(format!(
            "{}{endpoint}&q=path%3Adocs&cursor={cursor}",
            h.origin
        ))
        .header(reqwest::header::COOKIE, &reader)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI with native Git"]
async fn native_browser_finds_symbols_across_repositories_and_opens_the_definition() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let alice = h.login().await;
    let mut head = String::new();
    for name in ["private", "library"] {
        (_, _, head) = searchable_repository(
            &h,
            &alice,
            "team",
            name,
            [(
                format!("{name}.rs"),
                "// workspace_needle\nfn workspace_needle() {}\n".into(),
            )],
        )
        .await;
    }
    for repo in h.server.repositories.values() {
        code_index::reconcile(&h.server, &repo, &h.server.cancellation)
            .await
            .unwrap();
    }
    let browser = crate::test_support::http::browser::Browser::new("workspace-code-search");
    browser.run(&["open", &h.origin]).await;
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
        ])
        .await;
    browser.run(&["wait", "a[href='/-/code-search']"]).await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Search code across repositories",
        ])
        .await;
    browser.run(&["wait", "#workspace-search-text"]).await;
    browser
        .run(&["select", "#workspace-search-kind", "symbol"])
        .await;
    browser
        .run(&["fill", "#workspace-search-text", "workspace_needle"])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "private.rs"]).await;
    browser
        .run(&["wait", "--text", "Index used for 2 repositories"])
        .await;
    browser
        .run(&[
            "fill",
            "#workspace-search-text",
            "repo:team/library language:rust path:*.rs symbol:workspace_needle",
        ])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Index used for 1 repository"])
        .await;
    let link = format!("a[href*='/blob/{head}/library.rs'][href$='#L2']");
    browser.run(&["wait", &link]).await;
    browser.run(&["click", &link]).await;
    browser
        .run(&["wait", "--url", &format!("**/blob/{head}/library.rs**#L2")])
        .await;
    drop(browser);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn persistent_code_index_prunes_a_sparse_search_and_refreshes_after_publication() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (source, remote, head) = searchable_repository(
        &h,
        &alice,
        "team",
        "private",
        (0..512)
            .map(|i| {
                (
                    format!("src/part-{i:03}.rs"),
                    "fn ordinary() { stable_unique_search_symbol(); }\n".into(),
                )
            })
            .chain([
                (
                    "src/special.rs".into(),
                    "let message = \"Kelvin\";\n".into(),
                ),
                (
                    "src/target.rs".into(),
                    "fn stable_unique_search_symbol() { let stable_unique_search_target = 42; }\n"
                        .into(),
                ),
            ]),
    )
    .await;
    let repo = h.server.repositories.values()[0].clone();
    let query = "/api/code-search?q=stable_unique_search_target&directory=src";
    let fallback = h.json(query, &alice).await;
    assert_eq!(fallback["scanning_repositories"], 1);
    assert_eq!(fallback["scanned"], 32);
    assert!(fallback["next"].is_string());

    let before = std::time::Instant::now();
    code_index::reconcile(&h.server, &repo, &h.server.cancellation)
        .await
        .unwrap();
    let build_elapsed = before.elapsed();
    let before = std::time::Instant::now();
    let page = h.json(query, &alice).await;
    eprintln!(
        "indexed 514 paths in {build_elapsed:?}; sparse search completed in {:?}",
        before.elapsed()
    );
    assert_eq!(page["scanned"], 514);
    assert_eq!(page["verified"], 2);
    assert_eq!(page["groups"][0]["commit"], head);
    assert_eq!(page["groups"][0]["files"][0]["path"], "src/target.rs");
    assert_eq!(page["indexed_repositories"], 1);
    assert_eq!(page["next"], Value::Null);

    let symbols = h
        .json(
            "/api/code-search?q=stable_unique_search_symbol&kind=symbol",
            &alice,
        )
        .await;
    assert_eq!(symbols["verified"], 1);
    assert_eq!(symbols["groups"][0]["files"][0]["path"], "src/target.rs");
    assert_eq!(
        symbols["groups"][0]["files"][0]["lines"][0]["before"],
        "fn "
    );
    let by_revision = h.json(&format!("/api/repos/team/private/code-search?q=stable_unique_search_symbol&kind=symbol&rev={head}"), &alice).await;
    assert_eq!(by_revision["commit"], head);
    assert_eq!(by_revision["items"].as_array().unwrap().len(), 1);

    let unicode = h.json("/api/code-search?q=kelvin", &alice).await;
    assert_eq!(
        unicode["groups"][0]["files"][0]["lines"][0]["matched"],
        "Kelvin"
    );
    assert_eq!(unicode["indexed_repositories"], 1);
    let short = h.json("/api/code-search?q=42&directory=src", &alice).await;
    assert_eq!(short["verified"], 32);
    assert!(short["next"].is_string());

    let catalog = h.json("/api/repos?owner=team&name=private", &alice).await;
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/branch-protections",
        &alice,
        json!({"expected_version":catalog["repositories"][0]["protection_version"],"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    std::fs::write(
        source.path().join("src/target.rs"),
        "let replacement_needle = 41;\n",
    )
    .unwrap();
    git::success(source.path(), &["commit", "-am", "Change indexed text"]).await;
    let replacement = git::success(source.path(), &["rev-parse", "HEAD"]).await;
    push_workspace_fixture(source.path(), &remote).await;
    let stale = h.json(query, &alice).await;
    assert_eq!(stale["scanning_repositories"], 1);
    assert_eq!(stale["groups"], json!([]));

    code_index::reconcile(&h.server, &repo, &h.server.cancellation)
        .await
        .unwrap();
    let latest = h
        .json("/api/code-search?q=replacement_needle", &alice)
        .await;
    assert_eq!(latest["groups"][0]["commit"], replacement);
    assert_eq!(latest["groups"][0]["files"][0]["path"], "src/target.rs");
    assert_eq!(latest["indexed_repositories"], 1);
    let absent = h.json(query, &alice).await;
    assert_eq!(absent["groups"], json!([]));
    assert_eq!(absent["next"], Value::Null);
    h.close().await;
}
