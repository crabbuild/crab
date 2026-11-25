use std::path::Path;

use super::{Browser, Harness};
use crate::test_support::git::success as git;

pub(super) async fn push_base(path: &Path, upstream: &str, base_tip: &str) -> String {
    git(path, &["checkout", "-b", "stable", base_tip]).await;
    std::fs::write(path.join("RELEASE.txt"), "stable branch content\n").unwrap();
    git(path, &["add", "RELEASE.txt"]).await;
    git(path, &["commit", "-m", "Prepare stable base"]).await;
    git(path, &["push", upstream, "HEAD:refs/heads/stable"]).await;
    let oid = git(path, &["rev-parse", "HEAD"]).await;
    git(path, &["checkout", "main"]).await;
    oid
}

pub(super) async fn change_base(
    h: &Harness,
    browser: &Browser,
    alice: &str,
    pull: &str,
    (main, stable): (&str, &str),
) -> u64 {
    let current = h.json(pull, alice).await;
    let destinations = [
        ("stable", stable, "changed the base from main to stable"),
        ("main", main, "changed the base from stable to main"),
    ];
    let mut revision = 0;
    for (branch, oid, event) in destinations {
        browser
            .run(&[
                "find",
                "role",
                "button",
                "click",
                "--name",
                "Change base",
                "--exact",
            ])
            .await;
        let reference = format!("refs/heads/{branch}");
        browser.choose_select("#change-pull-base", &reference).await;
        browser.run(&["wait", "--text", "2 changed files"]).await;
        let preview = browser
            .run(&["get", "text", "[aria-label='New comparison']"])
            .await;
        assert!(preview.contains(&oid[..7]));
        assert!(preview.contains("CONTRIBUTION.txt"));
        assert!(!preview.contains("RELEASE.txt"));
        browser
            .run(&[
                "find",
                "role",
                "button",
                "click",
                "--name",
                "Confirm base change",
                "--exact",
            ])
            .await;
        browser.run(&["wait", "--text", event]).await;
        browser
            .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
            .await;
        let updated = h.json(pull, alice).await;
        assert_eq!(updated["base_ref"], reference);
        assert_eq!(updated["base_oid"], oid);
        assert_eq!(updated["head_oid"], current["head_oid"]);
        assert_eq!(updated["original_base_oid"], current["original_base_oid"]);
        revision = updated["base_revision"].as_u64().unwrap();
    }
    revision
}
