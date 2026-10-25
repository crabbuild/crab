#![cfg(feature = "local")]

use std::path::Path;

use crate::local_support::{commit, git_path, run};

#[path = "../support/capsule_fixture.rs"]
pub mod capsule_fixture;

fn git<S: AsRef<std::ffi::OsStr> + std::fmt::Debug>(root: &Path, args: &[S]) -> String {
    let output = std::process::Command::new(git_path())
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub async fn direct_remote(root: &Path, prefix: &str, commit_count: usize) -> Vec<String> {
    use crab_storage::{Store, StoreLayout};
    use std::{collections::BTreeMap, sync::Arc};
    let source = tempfile::tempdir().unwrap();
    run(
        &git_path(),
        source.path(),
        &["init", "--initial-branch=main"],
    );
    let mut commits = Vec::new();
    for index in 0..commit_count {
        commits.push(commit(
            &git_path(),
            source.path(),
            &format!("{index}.txt"),
            &format!("commit {index}"),
        ));
    }
    run(&git_path(), source.path(), &["tag", "retained"]);
    if commit_count > 1 {
        run(&git_path(), source.path(), &["tag", "removed", "HEAD~1"]);
    }
    let base = source.path().join("fixture");
    let git_sha = run(
        &git_path(),
        source.path(),
        &[
            "pack-objects",
            "--all",
            "--index-version=2",
            base.to_str().unwrap(),
        ],
    );
    let pack = source.path().join(format!("fixture-{git_sha}.pack"));
    std::fs::create_dir_all(root).unwrap();
    // Local filesystem inputs are read-only fixtures; encode with an in-memory
    // publisher, then copy its closed authority into the selected directory.
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), prefix.to_owned());
    let mut native =
        capsule_fixture::CapsuleFixture::new(layout, source.path(), &pack, &commits).await;
    let tip = commits.last().unwrap().clone();
    let mut refs = BTreeMap::from([
        ("refs/heads/main".to_owned(), tip.clone()),
        ("refs/tags/retained".to_owned(), tip),
    ]);
    if commit_count > 1 {
        refs.insert(
            "refs/tags/removed".to_owned(),
            commits[commit_count - 2].clone(),
        );
    }
    native.publish(refs, Default::default()).await;
    use futures_util::TryStreamExt as _;
    use object_store::ObjectStoreExt as _;
    for object in store
        .inner()
        .list(None)
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
    {
        let body = store
            .inner()
            .get(&object.location)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let path = root.join(object.location.as_ref());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    commits
}
