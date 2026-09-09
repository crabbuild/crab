pub mod read_fixture;

use crab_metadata::git_object_locator::{
    GitLocatorCoverage, GitObjectLocatorEntry, GitObjectLocatorWriter, GitPackLocatorRecord,
};
use crab_storage::Store;
use futures_util::TryStreamExt;
use object_store::ObjectStoreExt;
use std::path::Path;
use std::process::Command;
pub fn git<S: AsRef<std::ffi::OsStr> + std::fmt::Debug>(directory: &Path, args: &[S]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "user.name=SDK fixture",
            "-c",
            "user.email=sdk@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
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

pub async fn publish_catalog(
    store: &Store,
    coverage: GitLocatorCoverage,
    record: GitPackLocatorRecord,
    entries: &[GitObjectLocatorEntry],
) {
    let mut writer = GitObjectLocatorWriter::open(store.inner().clone(), "repository")
        .await
        .unwrap();
    let binding = writer.bind_packs(&[record]).await.unwrap()[0];
    writer.write_locations(binding, entries).await.unwrap();
    writer.set_coverage(coverage).await.unwrap();
    writer.close().await.unwrap();
}

pub async fn copy_published_objects(
    store: &Store,
    root: &Path,
    cloud: Option<&Store>,
    published: &mut std::collections::HashSet<object_store::path::Path>,
) {
    // LocalFileSystem is a read fixture, not the CAS publication backend.
    // Copy closed, published catalog objects instead of changing CAS semantics.
    let objects = store
        .inner()
        .list(None)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    for object in objects {
        let bytes = store
            .inner()
            .get(&object.location)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        if let Some(cloud) = cloud {
            // This fixture replaces a closed published snapshot between reads;
            // it does not exercise the production CAS publication protocol.
            cloud
                .put_overwrite(&object.location, bytes.clone())
                .await
                .unwrap();
        }
        published.insert(object.location.clone());
        let path = root.join(object.location.as_ref());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}
