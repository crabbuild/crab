use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use bytes::Bytes;
use crab_metadata::capsule_protocol::CapsuleGitPack;

pub(crate) struct GitHistory {
    pub(crate) oids: Vec<String>,
    pub(crate) pack: CapsuleGitPack,
}

pub(crate) fn history(commit_count: usize) -> GitHistory {
    let workspace = tempfile::tempdir().unwrap();
    let git_dir = workspace.path().join("repository.git");
    initialize(&git_dir);
    let tree = output(
        &git_dir,
        &["hash-object", "-t", "tree", "-w", "--stdin"],
        b"",
    );
    let mut oids: Vec<String> = Vec::with_capacity(commit_count);
    for sequence in 0..commit_count {
        let mut arguments = vec!["commit-tree", tree.as_str()];
        if let Some(parent) = oids.last() {
            arguments.extend(["-p", parent.as_str()]);
        }
        let timestamp = format!("@{} +0000", sequence + 1);
        oids.push(output_with_env(
            &git_dir,
            &arguments,
            format!("commit {sequence}\n").as_bytes(),
            &timestamp,
        ));
    }
    finish(workspace, git_dir, oids)
}

pub(crate) fn history_with_blob(body: &[u8]) -> GitHistory {
    let workspace = tempfile::tempdir().unwrap();
    let git_dir = workspace.path().join("repository.git");
    initialize(&git_dir);
    let blob = output(&git_dir, &["hash-object", "-w", "--stdin"], body);
    let tree = output(
        &git_dir,
        &["mktree"],
        format!("100644 blob {blob}\tasset\n").as_bytes(),
    );
    let commit = output(&git_dir, &["commit-tree", &tree, "-m", "asset"], b"");
    finish(workspace, git_dir, vec![commit])
}

pub(crate) async fn publish_blob(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    body: &[u8],
) {
    let base = crab_write::capsule_protocol::initialize(
        layout,
        blake3::hash(b"team-project").to_hex().as_ref(),
        "refs/heads/main",
    )
    .await
    .unwrap();
    let history = history_with_blob(body);
    let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
        base.record().digest(),
        vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some(history.oids[0].clone()),
            None,
        )],
    )
    .unwrap();
    let capsule = crab_metadata::capsule_protocol::Capsule::build(
        &transaction,
        vec![history.pack],
        Vec::new(),
    )
    .unwrap();
    crab_write::capsule_protocol::publish(layout, base, &transaction, &capsule)
        .await
        .unwrap();
}

pub(crate) async fn publish_blob_at_current(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    body: &[u8],
) {
    let base = crab_metadata::capsule_protocol::load_root(layout)
        .await
        .unwrap();
    let old = base.record().root().refs().get("refs/heads/main").cloned();
    let history = history_with_blob(body);
    let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
        base.record().digest(),
        vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            old,
            Some(history.oids[0].clone()),
            None,
        )],
    )
    .unwrap();
    let capsule = crab_metadata::capsule_protocol::Capsule::build(
        &transaction,
        vec![history.pack],
        Vec::new(),
    )
    .unwrap();
    crab_write::capsule_protocol::publish(layout, base, &transaction, &capsule)
        .await
        .unwrap();
}

fn initialize(git_dir: &Path) {
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(git_dir)
            .status()
            .unwrap()
            .success()
    );
}

fn finish(
    workspace: tempfile::TempDir,
    git_dir: std::path::PathBuf,
    oids: Vec<String>,
) -> GitHistory {
    assert!(
        Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["update-ref", "refs/heads/main"])
            .arg(oids.last().unwrap())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["repack", "-a", "-d", "--depth=64"])
            .status()
            .unwrap()
            .success()
    );
    let source_pack = std::fs::read_dir(git_dir.join("objects/pack"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "pack")
        })
        .unwrap();
    let pack_bytes = std::fs::read(&source_pack).unwrap();
    let installed_dir = workspace.path().join("installed");
    std::fs::create_dir_all(&installed_dir).unwrap();
    let installed = crab_git::pack::install_pack_file_from_path(
        &installed_dir,
        &source_pack,
        blake3::hash(&pack_bytes).to_hex().as_ref(),
        64 * 1024 * 1024,
        true,
    )
    .unwrap();
    let mut locations = crab_git::pack_locator::PackLocationIter::open(
        &installed.idx_path,
        &installed.rev_path,
        pack_bytes.len() as u64,
    )
    .unwrap();
    let object_count = locations.object_count();
    let object_ids = locations
        .by_ref()
        .map(|location| location.unwrap().oid)
        .collect::<Vec<_>>();
    let kinds = crab_git::pack::object_kinds_from_git_dir(&git_dir, &object_ids).unwrap();
    let ordered_kinds = object_ids
        .iter()
        .map(|oid| *kinds.get(oid).unwrap())
        .collect::<Vec<_>>();
    let checksum = gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).unwrap();
    let locator =
        crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds).unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from(pack_bytes),
        Bytes::from(std::fs::read(&installed.idx_path).unwrap()),
        Bytes::from(std::fs::read(&installed.rev_path).unwrap()),
        Bytes::from(locator),
        installed.git_sha1,
        object_count,
    )
    .unwrap();
    GitHistory { oids, pack }
}

fn output(git_dir: &Path, arguments: &[&str], input: &[u8]) -> String {
    output_with_env(git_dir, arguments, input, "@1 +0000")
}

fn output_with_env(git_dir: &Path, arguments: &[&str], input: &[u8], timestamp: &str) -> String {
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(arguments)
        .env("GIT_AUTHOR_NAME", "Crab Test")
        .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
        .env("GIT_AUTHOR_DATE", timestamp)
        .env("GIT_COMMITTER_NAME", "Crab Test")
        .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
        .env("GIT_COMMITTER_DATE", timestamp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}
