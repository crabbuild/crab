use super::*;
use crate::git::receive::hold_policy_lease;

#[tokio::test(flavor = "multi_thread")]
async fn force_pushes_recheck_rules_and_locked_paths_before_publication() {
    let (store, gate) = hold_policy_lease("test");
    let h = Harness::new_with_store(false, false, store).await;
    let alice = h.login().await;
    let (work, remote, base) = seed(&h, &alice).await;
    let path = work.path();
    let git = async |args: &[&str]| git::success(path, args).await;
    let head_ref = "refs/heads/topic/review";
    git(&["switch", "-c", "topic/review"]).await;
    std::fs::write(path.join("locked.bin"), "protected content\n").unwrap();
    git(&["add", "locked.bin"]).await;
    git(&["commit", "-m", "Add a file under review"]).await;
    git(&["push", remote.as_str(), "topic/review"]).await;
    let head = git(&["rev-parse", "HEAD"]).await;
    git(&["push", remote.as_str(), "HEAD:refs/heads/topic/retained"]).await;
    let lease = format!("--force-with-lease={head_ref}:{head}");
    let update = format!("{base}:{head_ref}");
    let undo = ["push", &lease, remote.as_str(), &update];
    let current_head = async || git(&["ls-remote", remote.as_str(), head_ref]).await;
    let latest = format!("{head}\t{head_ref}");
    let denied = git::git(path, &undo).await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("non-fast-forward update rejected"));
    assert_eq!(current_head().await, latest);

    let mut allow = rule("Allow rebasing topic branches", "branch", "topic/**");
    allow["force_pushes"] = json!("allow");
    let mut block = rule("Freeze this topic", "branch", "topic/review");
    block["force_pushes"] = json!("block");
    let save = async |version: u64, rules: Vec<Value>| {
        h.write(
            Method::PUT,
            RULESETS,
            &alice,
            json!({"expected_version":version,"rulesets":rules}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    save(0, vec![allow.clone()]).await;

    // Both tips already exist on the server. The rewrite must still catch
    // the removal of a locked file in the old branch tip.
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let locks = crab_lfs::LfsLockManager::lfs(repo.store.clone(), &repo.config.prefix);
    locks.lock("locked.bin", "bob-id").await.unwrap();
    let denied = git::git(path, &undo).await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("path is locked by another user"));
    assert_eq!(current_head().await, latest);
    locks.force_unlock("locked.bin").await.unwrap();

    gate.arm();
    let push = git::git(path, &undo);
    tokio::pin!(push);
    tokio::select! {
        _ = gate.reached() => (),
        output = &mut push => panic!("push finished before the policy change: {:?}", output.status),
    }
    save(1, vec![allow.clone(), block.clone()]).await;
    gate.resume();
    let denied = push.await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("ruleset blocks force pushes"));
    assert_eq!(current_head().await, latest);

    block["bypass_actors"] = json!([
        {"actor":{"kind":"repository_admin"},"mode":"always"}
    ]);
    save(2, vec![allow, block]).await;
    git(&undo).await;
    assert_eq!(current_head().await, format!("{base}\t{head_ref}"));
    // The client cannot replace a branch that moved beyond its expected tip.
    let denied = git::git(
        path,
        &[
            "push",
            &lease,
            remote.as_str(),
            "HEAD:refs/heads/topic/review",
        ],
    )
    .await;
    assert!(!denied.status.success());
    assert_eq!(current_head().await, format!("{base}\t{head_ref}"));

    // Reapply the identical update after restoring its old tip. Its
    // historical receipt cannot stand in for the new publication.
    git(&["push", remote.as_str(), "topic/review"]).await;
    assert_eq!(current_head().await, latest);
    git(&undo).await;
    assert_eq!(current_head().await, format!("{base}\t{head_ref}"));
    h.close().await;
}
