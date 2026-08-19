//! Git workspace orchestration for protected-push receive.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crab_auth::{PushRefUpdate, normalize_optional_oid};
use crab_git::pack::canonical_pack_id_from_object_filename;
use crab_metadata::{
    manifest_store,
    manifests::{Manifest, PackManifestEntry, validate_pack_manifest_entry},
    pack_metadata::PackMetadata,
};
use crab_storage::{Store, StoreLayout};
use object_store::path::Path as ObjectPath;

use crate::error::{AuthServerError, Result};

use super::{
    MaterializedSourcePush, ProtectedPushPlan, PushPrepareRecord, conflict, derive_peeled_refs,
    invalid, validate_ref_update, validate_sha1,
};

pub(super) async fn compute_changed_paths(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    plan: &ProtectedPushPlan,
    ref_updates: &[PushRefUpdate],
    prepare: Option<&PushPrepareRecord>,
) -> Result<Vec<String>> {
    GitReceiveWorkspace::new(store, router, repo_prefix)
        .compute_changed_paths(plan, ref_updates, prepare)
        .await
}

pub(super) async fn materialize_source_push(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    base: Option<&Manifest>,
    plan: &ProtectedPushPlan,
    source_updates: &[PushRefUpdate],
    prepare: &PushPrepareRecord,
) -> Result<MaterializedSourcePush> {
    if plan.ref_updates.len() != source_updates.len() {
        return Err(invalid("source ref updates do not match push plan"));
    }
    GitReceiveWorkspace::new(store, router, repo_prefix)
        .materialize_source_push(base, plan, source_updates, prepare)
        .await
}

pub(super) async fn install_base_packs(
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
) -> Result<()> {
    GitReceiveWorkspace::new(store, router, router.repo_prefix())
        .install_base_packs(git_dir)
        .await
}

pub(super) async fn install_manifest_packs(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    git_dir: &Path,
) -> Result<()> {
    GitReceiveWorkspace::new(store, router, router.repo_prefix())
        .install_manifest_packs(router, manifest, git_dir)
        .await
}

struct CommitIdentity {
    author_name: String,
    author_email: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_date: String,
    message: Vec<u8>,
}

struct GitReceiveWorkspace<'a> {
    store: &'a Store,
    router: &'a StoreLayout<Store>,
    repo_prefix: &'a str,
}

impl<'a> GitReceiveWorkspace<'a> {
    fn new(store: &'a Store, router: &'a StoreLayout<Store>, repo_prefix: &'a str) -> Self {
        Self {
            store,
            router,
            repo_prefix,
        }
    }

    async fn materialize_source_push(
        &self,
        base: Option<&Manifest>,
        plan: &ProtectedPushPlan,
        source_updates: &[PushRefUpdate],
        prepare: &PushPrepareRecord,
    ) -> Result<MaterializedSourcePush> {
        let temp = tempfile::tempdir()?;
        let git_dir = temp.path().join("source.git");
        run_git(["init", "--bare", path_str(&git_dir)?], None)?;
        self.install_base_packs(&git_dir).await?;
        self.install_prepared_view_packs(prepare, &git_dir).await?;
        self.install_staged_packs(plan, &git_dir).await?;
        let view_old_refs = self.prepared_view_old_refs(prepare).await?;

        let mut final_updates = Vec::with_capacity(plan.ref_updates.len());
        let mut synthesized_tips = Vec::new();
        for (view_update, source_update) in plan.ref_updates.iter().zip(source_updates) {
            let source_old = source_update.old_oid.as_deref();
            let view_old = self.effective_view_old_oid(view_update, &view_old_refs);
            if source_old == view_old.as_deref()
                && self.is_fast_forward(&git_dir, source_old, &view_update.new_oid)?
            {
                final_updates.push(source_update.clone());
                continue;
            }

            let source_new = self.synthesize_source_commit(
                &git_dir,
                temp.path(),
                source_old,
                view_old.as_deref(),
                &view_update.new_oid,
            )?;
            synthesized_tips.push((source_new.clone(), source_old.map(ToOwned::to_owned)));
            final_updates.push(PushRefUpdate {
                ref_name: source_update.ref_name.clone(),
                old_oid: source_update.old_oid.clone(),
                new_oid: source_new,
            });
        }

        let packs = if synthesized_tips.is_empty() {
            Vec::new()
        } else {
            vec![
                self.upload_synthesized_pack(&git_dir, temp.path(), &synthesized_tips)
                    .await?,
            ]
        };
        validate_git_publication(&git_dir, &final_updates)?;
        if let Some(base) = base {
            for update in &final_updates {
                if update.old_oid.as_deref() != base.refs.get(&update.ref_name).map(String::as_str)
                {
                    return Err(conflict(format!(
                        "ref changed since source materialization: {}",
                        update.ref_name
                    )));
                }
            }
        }
        let mut final_refs = base.map_or_else(BTreeMap::new, |manifest| manifest.refs.clone());
        for update in &final_updates {
            if update.new_oid.is_empty() {
                final_refs.remove(&update.ref_name);
            } else {
                final_refs.insert(update.ref_name.clone(), update.new_oid.clone());
            }
        }
        let final_refs = final_refs.into_iter().collect::<Vec<_>>();
        let peeled_refs = derive_peeled_refs(&git_dir, &final_refs)?;
        Ok(MaterializedSourcePush {
            ref_updates: final_updates,
            packs,
            peeled_refs,
        })
    }

    fn is_fast_forward(
        &self,
        git_dir: &Path,
        old_oid: Option<&str>,
        new_oid: &str,
    ) -> Result<bool> {
        self.require_commit_object(git_dir, new_oid, "new_oid")?;
        let Some(old_oid) = old_oid else {
            return Ok(true);
        };
        self.require_commit_object(git_dir, old_oid, "old_oid")?;
        let output = Command::new("git")
            .args([
                "--git-dir",
                path_str(git_dir)?,
                "merge-base",
                "--is-ancestor",
                old_oid,
                new_oid,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(invalid(String::from_utf8_lossy(&output.stderr).trim())),
        }
    }

    fn synthesize_source_commit(
        &self,
        git_dir: &Path,
        temp_root: &Path,
        source_old: Option<&str>,
        view_old: Option<&str>,
        view_new: &str,
    ) -> Result<String> {
        self.require_commit_object(git_dir, view_new, "new_oid")?;
        if let Some(old) = view_old {
            self.require_commit_object(git_dir, old, "old_oid")?;
        }
        if let Some(old) = source_old {
            self.require_commit_object(git_dir, old, "source old_oid")?;
        }

        let index_path = temp_root.join(format!("synth-{}.index", blake3_hex(view_new.as_bytes())));
        if source_old.is_some() {
            run_git_with_env(
                [
                    "--git-dir",
                    path_str(git_dir)?,
                    "read-tree",
                    &format!("{}^{{tree}}", source_old.unwrap_or_default()),
                ],
                [("GIT_INDEX_FILE", path_str(&index_path)?)],
            )?;
        } else {
            run_git_with_env(
                ["--git-dir", path_str(git_dir)?, "read-tree", "--empty"],
                [("GIT_INDEX_FILE", path_str(&index_path)?)],
            )?;
        }

        let update = PushRefUpdate {
            ref_name: "refs/heads/synthetic".to_owned(),
            old_oid: view_old.map(ToOwned::to_owned),
            new_oid: view_new.to_owned(),
        };
        for path in self.ref_tree_changed_paths(git_dir, &update)? {
            if let Some(entry) = ls_tree_entry(git_dir, view_new, &path)? {
                run_git_with_env(
                    [
                        "--git-dir",
                        path_str(git_dir)?,
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &entry.mode,
                        &entry.oid,
                        &path,
                    ],
                    [("GIT_INDEX_FILE", path_str(&index_path)?)],
                )?;
            } else {
                run_git_with_env(
                    [
                        "--git-dir",
                        path_str(git_dir)?,
                        "update-index",
                        "--force-remove",
                        "--",
                        &path,
                    ],
                    [("GIT_INDEX_FILE", path_str(&index_path)?)],
                )?;
            }
        }

        let tree = run_git_capture_with_env(
            ["--git-dir", path_str(git_dir)?, "write-tree"],
            [("GIT_INDEX_FILE", path_str(&index_path)?)],
        )?
        .trim()
        .to_owned();
        let identity = commit_identity(git_dir, view_new)?;
        let message_path =
            temp_root.join(format!("synth-{}.message", blake3_hex(view_new.as_bytes())));
        fs::write(&message_path, &identity.message)?;
        let mut args = vec![
            "--git-dir".to_owned(),
            path_str(git_dir)?.to_owned(),
            "commit-tree".to_owned(),
            tree,
        ];
        if let Some(parent) = source_old {
            args.push("-p".to_owned());
            args.push(parent.to_owned());
        }
        args.push("-F".to_owned());
        args.push(path_str(&message_path)?.to_owned());
        run_git_owned_with_env(
            args,
            [
                ("GIT_AUTHOR_NAME", identity.author_name.as_str()),
                ("GIT_AUTHOR_EMAIL", identity.author_email.as_str()),
                ("GIT_AUTHOR_DATE", identity.author_date.as_str()),
                ("GIT_COMMITTER_NAME", identity.committer_name.as_str()),
                ("GIT_COMMITTER_EMAIL", identity.committer_email.as_str()),
                ("GIT_COMMITTER_DATE", identity.committer_date.as_str()),
            ],
        )
        .map(|out| out.trim().to_owned())
    }

    async fn upload_synthesized_pack(
        &self,
        git_dir: &Path,
        temp_root: &Path,
        tips: &[(String, Option<String>)],
    ) -> Result<PackManifestEntry> {
        let mut input = String::new();
        for (new_oid, old_oid) in tips {
            input.push_str(new_oid);
            input.push('\n');
            if let Some(old_oid) = old_oid {
                input.push('^');
                input.push_str(old_oid);
                input.push('\n');
            }
        }
        let pack_bytes = git_capture_bytes_with_input_owned(
            vec![
                "--git-dir".to_owned(),
                path_str(git_dir)?.to_owned(),
                "pack-objects".to_owned(),
                "--stdout".to_owned(),
                "--revs".to_owned(),
            ],
            input.as_bytes(),
        )?;
        let pack_id = blake3_hex(&pack_bytes);
        let pack_path = self.router.pack_path(&pack_id);
        self.store
            .put_exact(&pack_path, bytes::Bytes::from(pack_bytes.clone()))
            .await?;

        let object_count = rev_list_object_count(git_dir, tips)?;
        let metadata = PackMetadata {
            pack_id: pack_id.clone(),
            ref_tips: tips.iter().map(|(tip, _)| tip.clone()).collect(),
            object_count,
        };
        let metadata_path = self.router.pack_metadata_path(&pack_id);
        let metadata_bytes = serde_json::to_vec(&metadata)
            .map_err(|e| AuthServerError::Internal(format!("pack metadata serialize: {e}")))?;
        self.store
            .put_exact(&metadata_path, bytes::Bytes::from(metadata_bytes))
            .await?;

        let verify_path = temp_root.join(format!("pack-{pack_id}.pack"));
        fs::write(&verify_path, &pack_bytes)?;
        run_git(
            ["index-pack", "--strict", path_str(&verify_path)?],
            Some(git_dir),
        )?;
        Ok(PackManifestEntry {
            pack_id: pack_id.clone(),
            size: pack_bytes.len() as u64,
            content_hash: pack_id,
            ref_tips: tips.iter().map(|(tip, _)| tip.clone()).collect(),
            object_count,
        })
    }

    async fn compute_changed_paths(
        &self,
        plan: &ProtectedPushPlan,
        ref_updates: &[PushRefUpdate],
        prepare: Option<&PushPrepareRecord>,
    ) -> Result<Vec<String>> {
        let temp = tempfile::tempdir()?;
        let git_dir = temp.path().join("repo.git");
        run_git(["init", "--bare", path_str(&git_dir)?], None)?;
        self.install_base_packs(&git_dir).await?;
        if let Some(prepare) = prepare {
            self.install_prepared_view_packs(prepare, &git_dir).await?;
        }
        self.install_staged_packs(plan, &git_dir).await?;
        let view_old_refs = match prepare {
            Some(prepare) => self.prepared_view_old_refs(prepare).await?,
            None => BTreeMap::new(),
        };
        for update in ref_updates {
            let update = self.effective_view_ref_update(update, &view_old_refs);
            self.validate_ref_graph(&git_dir, &update, !view_old_refs.is_empty())?;
        }
        validate_git_publication(&git_dir, ref_updates)?;
        let mut paths = BTreeSet::new();
        for update in ref_updates {
            let update = self.effective_view_ref_update(update, &view_old_refs);
            paths.extend(self.ref_tree_changed_paths(&git_dir, &update)?);
            for commit in self.introduced_commits(&git_dir, &update)? {
                paths.extend(self.commit_changed_paths(&git_dir, &commit)?);
            }
        }
        Ok(paths.into_iter().collect())
    }

    fn validate_ref_graph(
        &self,
        git_dir: &Path,
        update: &PushRefUpdate,
        allow_equivalent_old_tree: bool,
    ) -> Result<()> {
        self.require_commit_object(git_dir, &update.new_oid, "new_oid")?;
        let Some(old) = normalize_optional_oid(update.old_oid.as_deref()) else {
            return Ok(());
        };
        self.require_commit_object(git_dir, old.as_str(), "old_oid")?;
        match self.require_fast_forward(git_dir, update, old.as_str()) {
            Ok(()) => Ok(()),
            Err(AuthServerError::NonFastForward { .. })
                if allow_equivalent_old_tree
                    && self.history_contains_tree(git_dir, &update.new_oid, old.as_str())? =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn require_commit_object(&self, git_dir: &Path, oid: &str, field: &str) -> Result<()> {
        let output = run_git_capture(["--git-dir", path_str(git_dir)?, "cat-file", "-t", oid])?;
        if output.trim() != "commit" {
            return Err(invalid(format!("{field} must reference a commit object")));
        }
        Ok(())
    }

    fn require_fast_forward(
        &self,
        git_dir: &Path,
        update: &PushRefUpdate,
        old_oid: &str,
    ) -> Result<()> {
        let output = Command::new("git")
            .args([
                "--git-dir",
                path_str(git_dir)?,
                "merge-base",
                "--is-ancestor",
                old_oid,
                &update.new_oid,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;

        match output.status.code() {
            Some(0) => Ok(()),
            Some(1) => Err(AuthServerError::NonFastForward {
                ref_name: update.ref_name.clone(),
                have: old_oid.to_owned(),
                want: update.new_oid.clone(),
            }),
            _ => Err(invalid(String::from_utf8_lossy(&output.stderr).trim())),
        }
    }

    fn history_contains_tree(&self, git_dir: &Path, new_oid: &str, old_oid: &str) -> Result<bool> {
        let old_tree = run_git_capture([
            "--git-dir",
            path_str(git_dir)?,
            "rev-parse",
            &format!("{old_oid}^{{tree}}"),
        ])?;
        let trees = run_git_capture([
            "--git-dir",
            path_str(git_dir)?,
            "log",
            "--format=%T",
            new_oid,
        ])?;
        let old_tree = old_tree.trim();
        Ok(trees.lines().any(|tree| tree == old_tree))
    }

    fn ref_tree_changed_paths(
        &self,
        git_dir: &Path,
        update: &PushRefUpdate,
    ) -> Result<Vec<String>> {
        let output = if let Some(old) = normalize_optional_oid(update.old_oid.as_deref()) {
            run_git_capture_bytes([
                "--git-dir",
                path_str(git_dir)?,
                "diff",
                "--no-renames",
                "--name-only",
                "-z",
                old.as_str(),
                &update.new_oid,
            ])?
        } else {
            run_git_capture_bytes([
                "--git-dir",
                path_str(git_dir)?,
                "ls-tree",
                "-r",
                "--name-only",
                "-z",
                &update.new_oid,
            ])?
        };
        nul_paths(output)
    }

    fn introduced_commits(&self, git_dir: &Path, update: &PushRefUpdate) -> Result<Vec<String>> {
        let output = if let Some(old) = normalize_optional_oid(update.old_oid.as_deref()) {
            run_git_capture([
                "--git-dir",
                path_str(git_dir)?,
                "rev-list",
                &update.new_oid,
                "--not",
                old.as_str(),
            ])?
        } else {
            run_git_capture(["--git-dir", path_str(git_dir)?, "rev-list", &update.new_oid])?
        };
        Ok(lines(output))
    }

    fn commit_changed_paths(&self, git_dir: &Path, commit: &str) -> Result<Vec<String>> {
        let output = run_git_capture_bytes([
            "--git-dir",
            path_str(git_dir)?,
            "diff-tree",
            "--no-renames",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-m",
            "-z",
            commit,
        ])?;
        nul_paths(output)
    }

    async fn install_base_packs(&self, git_dir: &Path) -> Result<()> {
        let Ok((manifest, _)) = read_manifest(self.store, self.router).await else {
            return Ok(());
        };
        self.install_manifest_packs(self.router, &manifest, git_dir)
            .await
    }

    async fn install_prepared_view_packs(
        &self,
        prepare: &PushPrepareRecord,
        git_dir: &Path,
    ) -> Result<()> {
        let Some((router, manifest)) = self.read_prepared_view_manifest(prepare).await? else {
            return Ok(());
        };
        self.install_manifest_packs(&router, &manifest, git_dir)
            .await
    }

    async fn prepared_view_old_refs(
        &self,
        prepare: &PushPrepareRecord,
    ) -> Result<BTreeMap<String, String>> {
        let Some((_router, manifest)) = self.read_prepared_view_manifest(prepare).await? else {
            return Ok(BTreeMap::new());
        };
        for (ref_name, oid) in &manifest.refs {
            validate_ref_update(&PushRefUpdate {
                ref_name: ref_name.clone(),
                old_oid: None,
                new_oid: oid.clone(),
            })?;
        }
        Ok(manifest.refs)
    }

    async fn read_prepared_view_manifest(
        &self,
        prepare: &PushPrepareRecord,
    ) -> Result<Option<(StoreLayout<Store>, Manifest)>> {
        let Some(scope) = prepare.view_scope.as_ref() else {
            return Ok(None);
        };
        let router = StoreLayout::with_global_prefix(
            self.store.clone(),
            scope.repo_prefix.clone(),
            scope.global_prefix.clone(),
        );
        let (manifest, _) = read_manifest(self.store, &router)
            .await
            .map_err(|e| invalid(format!("prepared ACL view is not readable: {e}")))?;
        Ok(Some((router, manifest)))
    }

    fn effective_view_ref_update(
        &self,
        update: &PushRefUpdate,
        view_old_refs: &BTreeMap<String, String>,
    ) -> PushRefUpdate {
        PushRefUpdate {
            ref_name: update.ref_name.clone(),
            old_oid: self.effective_view_old_oid(update, view_old_refs),
            new_oid: update.new_oid.clone(),
        }
    }

    fn effective_view_old_oid(
        &self,
        update: &PushRefUpdate,
        view_old_refs: &BTreeMap<String, String>,
    ) -> Option<String> {
        view_old_refs
            .get(&update.ref_name)
            .cloned()
            .or_else(|| normalize_optional_oid(update.old_oid.as_deref()))
    }

    async fn install_manifest_packs(
        &self,
        router: &StoreLayout<Store>,
        manifest: &Manifest,
        git_dir: &Path,
    ) -> Result<()> {
        if manifest.pack_index_hash.is_empty() {
            return Ok(());
        }
        for pack in read_bulk_pack_list(self.store, router, &manifest.pack_index_hash).await? {
            validate_pack_manifest_entry(&pack)?;
            let path = router.pack_path(&pack.pack_id);
            self.install_pack_from_object(&path, git_dir).await?;
        }
        Ok(())
    }

    async fn install_staged_packs(&self, plan: &ProtectedPushPlan, git_dir: &Path) -> Result<()> {
        for object in &plan.staged_objects {
            if object
                .canonical_key
                .starts_with(&format!("{}/packs/", self.repo_prefix))
                && object.canonical_key.ends_with(".pack")
            {
                let path = ObjectPath::from(object.staged_key.clone());
                self.install_pack_from_object(&path, git_dir).await?;
            }
        }
        Ok(())
    }

    async fn install_pack_from_object(&self, path: &ObjectPath, git_dir: &Path) -> Result<()> {
        let file_name = path
            .as_ref()
            .rsplit('/')
            .next()
            .ok_or_else(|| invalid("pack path has no filename"))?;
        validate_pack_object_filename(file_name)?;
        let pack_path = git_dir.join("objects").join("pack").join(file_name);
        self.store.download_to_path(path, &pack_path).await?;
        run_git(["index-pack", path_str(&pack_path)?], Some(git_dir))?;
        Ok(())
    }
}

fn validate_git_publication(git_dir: &Path, updates: &[PushRefUpdate]) -> Result<()> {
    let mut args = vec![
        "--git-dir".to_owned(),
        path_str(git_dir)?.to_owned(),
        "fsck".to_owned(),
        "--strict".to_owned(),
        "--full".to_owned(),
        "--no-reflogs".to_owned(),
        "--no-dangling".to_owned(),
    ];
    args.extend(updates.iter().map(|update| update.new_oid.clone()));
    let output = Command::new("git")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(invalid(String::from_utf8_lossy(&output.stderr).trim()))
}

async fn read_manifest(store: &Store, router: &StoreLayout<Store>) -> Result<(Manifest, String)> {
    manifest_store::read_manifest(store, router)
        .await
        .map_err(AuthServerError::from)
}

async fn read_bulk_pack_list(
    store: &Store,
    router: &StoreLayout<Store>,
    hash: &str,
) -> Result<Vec<PackManifestEntry>> {
    manifest_store::read_bulk_pack_list(store, router, hash)
        .await
        .map_err(AuthServerError::from)
}

pub(super) fn validate_pack_object_filename(file_name: &str) -> Result<()> {
    if canonical_pack_id_from_object_filename(file_name).is_none() {
        return Err(invalid("pack object filename is invalid"));
    }
    Ok(())
}

struct TreeEntry {
    mode: String,
    oid: String,
}

fn ls_tree_entry(git_dir: &Path, commit: &str, path: &str) -> Result<Option<TreeEntry>> {
    let output = run_git_capture_bytes([
        "--git-dir",
        path_str(git_dir)?,
        "ls-tree",
        "-z",
        commit,
        "--",
        path,
    ])?;
    if output.is_empty() {
        return Ok(None);
    }
    let entry = output
        .split(|byte| *byte == 0)
        .next()
        .ok_or_else(|| invalid("git ls-tree returned no entry"))?;
    let tab = entry
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| invalid("git ls-tree entry is malformed"))?;
    let header = &entry[..tab];
    let header =
        std::str::from_utf8(header).map_err(|_| invalid("git ls-tree header was not UTF-8"))?;
    let mut parts = header.split_whitespace();
    let mode = parts
        .next()
        .ok_or_else(|| invalid("git ls-tree entry missing mode"))?;
    let _kind = parts
        .next()
        .ok_or_else(|| invalid("git ls-tree entry missing kind"))?;
    let oid = parts
        .next()
        .ok_or_else(|| invalid("git ls-tree entry missing object id"))?;
    validate_sha1(oid, "tree entry object id")?;
    Ok(Some(TreeEntry {
        mode: mode.to_owned(),
        oid: oid.to_owned(),
    }))
}

fn commit_identity(git_dir: &Path, commit: &str) -> Result<CommitIdentity> {
    let output = run_git_capture_bytes([
        "--git-dir",
        path_str(git_dir)?,
        "show",
        "-s",
        "--format=%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B",
        commit,
    ])?;
    let mut fields = output.splitn(7, |byte| *byte == 0);
    let mut next = |field: &str| -> Result<String> {
        let value = fields
            .next()
            .ok_or_else(|| invalid(format!("commit identity missing {field}")))?;
        String::from_utf8(value.to_vec())
            .map_err(|_| invalid(format!("commit identity {field} was not UTF-8")))
    };
    let author_name = next("author name")?;
    let author_email = next("author email")?;
    let author_date = next("author date")?;
    let committer_name = next("committer name")?;
    let committer_email = next("committer email")?;
    let committer_date = next("committer date")?;
    let message = fields
        .next()
        .ok_or_else(|| invalid("commit identity missing message"))?
        .to_vec();
    Ok(CommitIdentity {
        author_name,
        author_email,
        author_date,
        committer_name,
        committer_email,
        committer_date,
        message,
    })
}

fn rev_list_object_count(git_dir: &Path, tips: &[(String, Option<String>)]) -> Result<u64> {
    let mut input = String::new();
    for (new_oid, old_oid) in tips {
        input.push_str(new_oid);
        input.push('\n');
        if let Some(old_oid) = old_oid {
            input.push('^');
            input.push_str(old_oid);
            input.push('\n');
        }
    }
    let output = git_capture_string_with_input_owned(
        vec![
            "--git-dir".to_owned(),
            path_str(git_dir)?.to_owned(),
            "rev-list".to_owned(),
            "--objects".to_owned(),
            "--stdin".to_owned(),
        ],
        input.as_bytes(),
    )?;
    let mut objects = BTreeSet::new();
    for line in output.lines() {
        if let Some(oid) = line.split_whitespace().next() {
            validate_sha1(oid, "rev-list object id")?;
            objects.insert(oid.to_owned());
        }
    }
    Ok(objects.len() as u64)
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn run_git<const N: usize>(args: [&str; N], cwd: Option<&Path>) -> Result<()> {
    let mut command = Command::new("git");
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(invalid(String::from_utf8_lossy(&output.stderr).trim()))
}

fn run_git_with_env<const N: usize, const M: usize>(
    args: [&str; N],
    env: [(&str, &str); M],
) -> Result<()> {
    let mut command = Command::new("git");
    command
        .args(args)
        .envs(env)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(invalid(String::from_utf8_lossy(&output.stderr).trim()))
}

fn run_git_capture<const N: usize>(args: [&str; N]) -> Result<String> {
    let stdout = run_git_capture_bytes(args)?;
    String::from_utf8(stdout).map_err(|_| invalid("git output was not valid UTF-8"))
}

fn run_git_capture_bytes<const N: usize>(args: [&str; N]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(invalid(String::from_utf8_lossy(&output.stderr).trim()))
}

fn run_git_capture_with_env<const N: usize, const M: usize>(
    args: [&str; N],
    env: [(&str, &str); M],
) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr).trim()));
    }
    String::from_utf8(output.stdout).map_err(|_| invalid("git output was not valid UTF-8"))
}

fn run_git_owned_with_env<const M: usize>(
    args: Vec<String>,
    env: [(&str, &str); M],
) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr).trim()));
    }
    String::from_utf8(output.stdout).map_err(|_| invalid("git output was not valid UTF-8"))
}

fn git_capture_bytes_with_input_owned(args: Vec<String>, input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| invalid("git stdin missing"))?
        .write_all(input)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(output.stdout)
}

fn git_capture_string_with_input_owned(args: Vec<String>, input: &[u8]) -> Result<String> {
    let output = git_capture_bytes_with_input_owned(args, input)?;
    String::from_utf8(output).map_err(|_| invalid("git output was not valid UTF-8"))
}

pub(super) fn nul_paths(output: Vec<u8>) -> Result<Vec<String>> {
    let parts: Vec<&[u8]> = output.split(|byte| *byte == 0).collect();
    let mut paths = Vec::new();
    for (idx, part) in parts.iter().enumerate() {
        if part.is_empty() {
            if idx == parts.len().saturating_sub(1) {
                continue;
            }
            return Err(invalid("git emitted an empty changed path"));
        }
        paths.push(normalize_git_path(part)?);
    }
    Ok(paths)
}

fn normalize_git_path(raw: &[u8]) -> Result<String> {
    let path = String::from_utf8(raw.to_vec())
        .map_err(|_| invalid("git emitted a non-UTF-8 changed path"))?;
    if path.len() > 4096 {
        return Err(invalid("git changed path is too long"));
    }
    if path.trim() != path
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains("//")
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(invalid(format!("unsafe git changed path: {path}")));
    }
    Ok(path)
}

fn lines(output: String) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| invalid("temporary path is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receive::PreparedViewScope;
    use bytes::Bytes;
    use crab_metadata::segmented::{self, SegmentIndex, SegmentKind};
    use crab_storage::StagedWrite;
    use object_store::memory::InMemory;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn hash(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn oid(ch: char) -> String {
        std::iter::repeat_n(ch, 40).collect()
    }

    fn blake3_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    fn store_and_router() -> (Store, StoreLayout<Store>) {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        (store, router)
    }

    fn staged_object_for_bytes(canonical_key: String, bytes: &[u8]) -> StagedWrite {
        StagedWrite {
            staged_key: format!(
                "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/objects/{canonical_key}"
            ),
            canonical_key,
            blake3: blake3_hex(bytes),
            size: bytes.len() as u64,
        }
    }

    async fn put_staged(store: &Store, object: &StagedWrite, bytes: Bytes) -> Result<()> {
        store
            .put_exact(&ObjectPath::from(object.staged_key.clone()), bytes)
            .await?;
        Ok(())
    }

    fn git_capture_with_input<const N: usize>(
        args: [&str; N],
        cwd: &Path,
        input: &[u8],
    ) -> Result<Vec<u8>> {
        let mut child = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AuthServerError::Internal(format!("spawn git failed: {e}")))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| AuthServerError::Internal("git stdin missing".to_owned()))?
            .write_all(input)
            .map_err(|e| AuthServerError::Internal(format!("write git stdin failed: {e}")))?;
        let output = child
            .wait_with_output()
            .map_err(|e| AuthServerError::Internal(format!("git wait failed: {e}")))?;
        if !output.status.success() {
            return Err(AuthServerError::Internal(format!(
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(output.stdout)
    }

    fn pack_entry_for_bytes(
        pack_bytes: &[u8],
        ref_tips: Vec<String>,
        object_count: u64,
    ) -> PackManifestEntry {
        let pack_id = blake3_hex(pack_bytes);
        PackManifestEntry {
            pack_id: pack_id.clone(),
            size: pack_bytes.len() as u64,
            content_hash: pack_id,
            ref_tips,
            object_count,
        }
    }

    fn git_object_count(repo: &Path, revs: &str) -> Result<u64> {
        let output =
            git_capture_with_input(["rev-list", "--objects", "--stdin"], repo, revs.as_bytes())?;
        let mut objects = BTreeSet::new();
        for line in String::from_utf8(output)
            .map_err(|_| invalid("test git output was not UTF-8"))?
            .lines()
        {
            if let Some(oid) = line.split_whitespace().next() {
                objects.insert(oid.to_owned());
            }
        }
        Ok(objects.len() as u64)
    }

    async fn create_manifest(
        store: &Store,
        router: &StoreLayout<Store>,
        manifest: &Manifest,
    ) -> Result<()> {
        manifest_store::create_manifest(store, router, manifest)
            .await
            .map_err(AuthServerError::from)
    }

    #[test]
    fn nul_paths_parse_git_paths_without_rewriting() {
        let paths = nul_paths(b"src/lib.rs\0docs/my file.md\0".to_vec()).unwrap();

        assert_eq!(
            paths,
            vec!["src/lib.rs".to_owned(), "docs/my file.md".to_owned()]
        );
    }

    #[test]
    fn nul_paths_reject_unsafe_git_paths() {
        for raw in [
            b" src/lib.rs\0".as_slice(),
            b"src/lib.rs \0".as_slice(),
            b"/src/lib.rs\0".as_slice(),
            b"src//lib.rs\0".as_slice(),
            b"src/../secret\0".as_slice(),
            b"src/a\nb\0".as_slice(),
            b"src/\xff\0".as_slice(),
        ] {
            assert!(
                nul_paths(raw.to_vec()).is_err(),
                "expected {:?} to be rejected",
                raw
            );
        }
    }

    #[test]
    fn pack_object_filename_requires_canonical_pack_name() {
        let valid = format!("pack-{}.pack", hash('a'));
        assert!(validate_pack_object_filename(&valid).is_ok());

        for file_name in [
            "",
            "pack-.pack",
            "pack-not-a-hash.pack",
            "pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack",
            "pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.idx",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack",
        ] {
            assert!(
                validate_pack_object_filename(file_name).is_err(),
                "expected {file_name:?} to be rejected"
            );
        }
    }

    #[test]
    fn publication_validation_rejects_malformed_reachable_tree() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let git_dir = temp.path().join("repo.git");
        run_git(["init", "--bare", path_str(&git_dir)?], None)?;

        let mut tree_body = b"199999 invalid\0".to_vec();
        tree_body.extend_from_slice(&[0; 20]);
        let tree = String::from_utf8(git_capture_with_input(
            ["hash-object", "--literally", "-w", "-t", "tree", "--stdin"],
            &git_dir,
            &tree_body,
        )?)
        .map_err(|_| invalid("test tree oid was not UTF-8"))?;
        let commit_body = format!(
            "tree {}\nauthor Test <test@example.com> 1700000000 +0000\ncommitter Test <test@example.com> 1700000000 +0000\n\nmalformed tree\n",
            tree.trim()
        );
        let commit = String::from_utf8(git_capture_with_input(
            [
                "hash-object",
                "--literally",
                "-w",
                "-t",
                "commit",
                "--stdin",
            ],
            &git_dir,
            commit_body.as_bytes(),
        )?)
        .map_err(|_| invalid("test commit oid was not UTF-8"))?;
        let updates = [PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: None,
            new_oid: commit.trim().to_owned(),
        }];

        validate_git_publication(&git_dir, &updates)
            .expect_err("strict publication validation must reject malformed objects");
        Ok(())
    }

    #[tokio::test]
    async fn install_base_packs_rejects_invalid_pack_metadata() -> Result<()> {
        let (store, router) = store_and_router();
        let bad_pack = PackManifestEntry {
            pack_id: "../manifest".to_owned(),
            size: 4,
            content_hash: "../manifest".to_owned(),
            ref_tips: vec![oid('2')],
            object_count: 1,
        };
        let segment = segmented::build_segment(SegmentKind::Pack, 4, false, &[bad_pack])?
            .ok_or_else(|| invalid("test segment missing"))?;
        let index = segmented::append_segment(SegmentIndex::default(), segment.reference.clone());
        let index_object = segmented::build_index_object(SegmentKind::Pack, index)?;

        store
            .put_exact(
                &router.repo_path(&segment.reference.path),
                Bytes::from(segment.bytes),
            )
            .await?;
        store
            .put_exact(
                &router.repo_path(&segmented::index_relative_path(
                    SegmentKind::Pack,
                    &index_object.hash,
                )),
                Bytes::from(index_object.bytes),
            )
            .await?;

        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 4;
        manifest.refs.insert("refs/heads/main".to_owned(), oid('1'));
        manifest.shard_index_hash.clear();
        manifest.pack_index_hash = index_object.hash;
        manifest.seal_git_validation();
        create_manifest(&store, &router, &manifest).await?;

        let temp = tempfile::tempdir()?;
        let git_dir = temp.path().join("repo.git");
        let err = install_base_packs(&store, &router, &git_dir)
            .await
            .expect_err("corrupt base pack metadata must be rejected before installation");

        assert!(
            err.to_string().contains("pack metadata pack_id"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn compute_changed_paths_reads_staged_git_pack() -> Result<()> {
        let (store, router) = store_and_router();
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("work");
        run_git(["init", path_str(&repo)?], None)?;
        run_git(["config", "user.email", "alice@example.com"], Some(&repo))?;
        run_git(["config", "user.name", "Alice"], Some(&repo))?;
        std::fs::create_dir_all(repo.join("src"))?;
        std::fs::write(repo.join("src/lib.rs"), b"pub fn answer() -> u8 { 42 }\n")?;
        run_git(["add", "src/lib.rs"], Some(&repo))?;
        run_git(["commit", "-m", "initial"], Some(&repo))?;
        let commit = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &repo,
            format!("{commit}\n").as_bytes(),
        )?;

        let pack_id = blake3_hex(&pack_bytes);
        let pack_key = format!("org/repo/packs/pack-{pack_id}.pack");
        let pack_object = staged_object_for_bytes(pack_key, &pack_bytes);
        put_staged(&store, &pack_object, Bytes::from(pack_bytes)).await?;

        let mut candidate = Manifest::default_for_repo("refs/heads/main");
        candidate.generation = 1;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), commit.clone());
        candidate.seal_git_validation();
        let plan = ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: None,
            base_manifest_etag: None,
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: commit,
            }],
            candidate_manifest: candidate,
            push_commit_receipt: None,
            staged_objects: vec![pack_object],
        };

        let paths =
            compute_changed_paths(&store, &router, "org/repo", &plan, &plan.ref_updates, None)
                .await?;

        assert_eq!(paths, vec!["src/lib.rs".to_owned()]);
        Ok(())
    }

    #[tokio::test]
    async fn compute_changed_paths_reads_prepared_view_old_commit() -> Result<()> {
        let (store, router) = store_and_router();
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("work");
        run_git(["init", path_str(&repo)?], None)?;
        run_git(["config", "user.email", "alice@example.com"], Some(&repo))?;
        run_git(["config", "user.name", "Alice"], Some(&repo))?;
        std::fs::create_dir_all(repo.join("src"))?;
        std::fs::write(repo.join("src/lib.rs"), b"pub fn answer() -> u8 { 1 }\n")?;
        run_git(["add", "src/lib.rs"], Some(&repo))?;
        run_git(["commit", "-m", "view base"], Some(&repo))?;
        let old = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        std::fs::write(repo.join("src/lib.rs"), b"pub fn answer() -> u8 { 2 }\n")?;
        run_git(["add", "src/lib.rs"], Some(&repo))?;
        run_git(["commit", "-m", "view update"], Some(&repo))?;
        let new = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();

        let old_pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &repo,
            format!("{old}\n").as_bytes(),
        )?;
        let old_pack = pack_entry_for_bytes(
            &old_pack_bytes,
            vec![old.clone()],
            git_object_count(&repo, &format!("{old}\n"))?,
        );
        let view_scope = PreparedViewScope {
            repo_prefix: "org/repo/acl-views/v1/aaaaaaaa/1-deadbeef".to_owned(),
            global_prefix: "org/repo/acl-views/v1/aaaaaaaa/1-deadbeef/.crab".to_owned(),
            source_repo: "org/repo".to_owned(),
            scope_hash: hash('a'),
        };
        let view_router = StoreLayout::with_global_prefix(
            store.clone(),
            view_scope.repo_prefix.clone(),
            view_scope.global_prefix.clone(),
        );
        store
            .put_exact(
                &view_router.pack_path(&old_pack.pack_id),
                Bytes::from(old_pack_bytes),
            )
            .await?;
        let view_segment = segmented::build_segment(SegmentKind::Pack, 1, false, &[old_pack])?
            .ok_or_else(|| invalid("test view pack segment missing"))?;
        store
            .put_exact(
                &view_router.repo_path(&view_segment.reference.path),
                Bytes::from(view_segment.bytes),
            )
            .await?;
        let view_index = segmented::append_segment(SegmentIndex::default(), view_segment.reference);
        let view_index_object = segmented::build_index_object(SegmentKind::Pack, view_index)?;
        store
            .put_exact(
                &view_router.repo_path(&segmented::index_relative_path(
                    SegmentKind::Pack,
                    &view_index_object.hash,
                )),
                Bytes::from(view_index_object.bytes),
            )
            .await?;
        let mut view_manifest = Manifest::default_for_repo("refs/heads/main");
        view_manifest.generation = 1;
        view_manifest
            .refs
            .insert("refs/heads/main".to_owned(), old.clone());
        view_manifest.pack_index_hash = view_index_object.hash;
        view_manifest.seal_git_validation();
        create_manifest(&store, &view_router, &view_manifest).await?;

        let staged_pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &repo,
            format!("{new}\n^{old}\n").as_bytes(),
        )?;
        let pack_id = blake3_hex(&staged_pack_bytes);
        let pack_key = format!("org/repo/packs/pack-{pack_id}.pack");
        let pack_object = staged_object_for_bytes(pack_key, &staged_pack_bytes);
        put_staged(&store, &pack_object, Bytes::from(staged_pack_bytes)).await?;

        let mut candidate = Manifest::default_for_repo("refs/heads/main");
        candidate.generation = 1;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), new.clone());
        candidate.seal_git_validation();
        let plan = ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: Some(1),
            base_manifest_etag: Some("etag-1".to_owned()),
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(old),
                new_oid: new,
            }],
            candidate_manifest: candidate,
            push_commit_receipt: None,
            staged_objects: vec![pack_object],
        };
        let prepare = PushPrepareRecord {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            source_manifest_generation: Some(1),
            source_manifest_etag: Some("etag-1".to_owned()),
            view_ref_updates: plan.ref_updates.clone(),
            source_ref_updates: plan.ref_updates.clone(),
            view_scope: Some(view_scope),
        };

        let paths = compute_changed_paths(
            &store,
            &router,
            "org/repo",
            &plan,
            &plan.ref_updates,
            Some(&prepare),
        )
        .await?;

        assert_eq!(paths, vec!["src/lib.rs".to_owned()]);
        Ok(())
    }

    #[tokio::test]
    async fn compute_changed_paths_includes_both_sides_of_rename() -> Result<()> {
        let (store, router) = store_and_router();
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("work");
        run_git(["init", path_str(&repo)?], None)?;
        run_git(["config", "user.email", "alice@example.com"], Some(&repo))?;
        run_git(["config", "user.name", "Alice"], Some(&repo))?;
        std::fs::create_dir_all(repo.join("secret"))?;
        std::fs::write(repo.join("secret/prod.txt"), b"classified\n")?;
        run_git(["add", "secret/prod.txt"], Some(&repo))?;
        run_git(["commit", "-m", "base"], Some(&repo))?;
        let old = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();

        std::fs::create_dir_all(repo.join("src"))?;
        run_git(["mv", "secret/prod.txt", "src/prod.txt"], Some(&repo))?;
        run_git(["commit", "-m", "rename"], Some(&repo))?;
        let new = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();

        let pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &repo,
            format!("{old}\n{new}\n").as_bytes(),
        )?;
        let pack_id = blake3_hex(&pack_bytes);
        let pack_key = format!("org/repo/packs/pack-{pack_id}.pack");
        let pack_object = staged_object_for_bytes(pack_key, &pack_bytes);
        put_staged(&store, &pack_object, Bytes::from(pack_bytes)).await?;

        let mut candidate = Manifest::default_for_repo("refs/heads/main");
        candidate.generation = 5;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), new.clone());
        candidate.seal_git_validation();
        let plan = ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: Some(4),
            base_manifest_etag: Some("etag-1".to_owned()),
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(old),
                new_oid: new,
            }],
            candidate_manifest: candidate,
            push_commit_receipt: None,
            staged_objects: vec![pack_object],
        };

        let paths =
            compute_changed_paths(&store, &router, "org/repo", &plan, &plan.ref_updates, None)
                .await?;

        assert_eq!(
            paths,
            vec!["secret/prod.txt".to_owned(), "src/prod.txt".to_owned()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn compute_changed_paths_rejects_non_fast_forward_ref_update() -> Result<()> {
        let (store, router) = store_and_router();
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("work");
        run_git(["init", path_str(&repo)?], None)?;
        run_git(["config", "user.email", "alice@example.com"], Some(&repo))?;
        run_git(["config", "user.name", "Alice"], Some(&repo))?;

        std::fs::create_dir_all(repo.join("src"))?;
        std::fs::write(repo.join("src/lib.rs"), b"pub fn base() {}\n")?;
        run_git(["add", "src/lib.rs"], Some(&repo))?;
        run_git(["commit", "-m", "base"], Some(&repo))?;
        let base = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();

        std::fs::write(repo.join("src/lib.rs"), b"pub fn old_tip() {}\n")?;
        run_git(["add", "src/lib.rs"], Some(&repo))?;
        run_git(["commit", "-m", "old tip"], Some(&repo))?;
        let old = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();

        run_git(["checkout", "-b", "diverge", &base], Some(&repo))?;
        std::fs::write(repo.join("src/lib.rs"), b"pub fn rewritten_tip() {}\n")?;
        run_git(["add", "src/lib.rs"], Some(&repo))?;
        run_git(["commit", "-m", "rewritten tip"], Some(&repo))?;
        let new = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();

        let pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &repo,
            format!("{old}\n{new}\n").as_bytes(),
        )?;
        let pack_id = blake3_hex(&pack_bytes);
        let pack_key = format!("org/repo/packs/pack-{pack_id}.pack");
        let pack_object = staged_object_for_bytes(pack_key, &pack_bytes);
        put_staged(&store, &pack_object, Bytes::from(pack_bytes)).await?;

        let mut candidate = Manifest::default_for_repo("refs/heads/main");
        candidate.generation = 5;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), new.clone());
        candidate.seal_git_validation();
        let plan = ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: Some(4),
            base_manifest_etag: Some("etag-1".to_owned()),
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(old.clone()),
                new_oid: new.clone(),
            }],
            candidate_manifest: candidate,
            push_commit_receipt: None,
            staged_objects: vec![pack_object],
        };

        let err =
            compute_changed_paths(&store, &router, "org/repo", &plan, &plan.ref_updates, None)
                .await
                .expect_err("server receive must reject non-fast-forward updates");

        assert!(matches!(
            err,
            AuthServerError::NonFastForward {
                ref_name,
                have,
                want
            } if ref_name == "refs/heads/main" && have == old && want == new
        ));
        Ok(())
    }

    #[tokio::test]
    async fn compute_changed_paths_rejects_non_commit_ref_tip() -> Result<()> {
        let (store, router) = store_and_router();
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("work");
        run_git(["init", path_str(&repo)?], None)?;
        run_git(["config", "user.email", "alice@example.com"], Some(&repo))?;
        run_git(["config", "user.name", "Alice"], Some(&repo))?;

        std::fs::create_dir_all(repo.join("src"))?;
        std::fs::write(repo.join("src/lib.rs"), b"pub fn answer() -> u8 { 42 }\n")?;
        run_git(["add", "src/lib.rs"], Some(&repo))?;
        run_git(["commit", "-m", "initial"], Some(&repo))?;
        let tree = run_git_capture(["-C", path_str(&repo)?, "rev-parse", "HEAD^{tree}"])?
            .trim()
            .to_owned();

        let pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout"],
            &repo,
            format!("{tree}\n").as_bytes(),
        )?;
        let pack_id = blake3_hex(&pack_bytes);
        let pack_key = format!("org/repo/packs/pack-{pack_id}.pack");
        let pack_object = staged_object_for_bytes(pack_key, &pack_bytes);
        put_staged(&store, &pack_object, Bytes::from(pack_bytes)).await?;

        let mut candidate = Manifest::default_for_repo("refs/heads/main");
        candidate.generation = 1;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), tree.clone());
        candidate.seal_git_validation();
        let plan = ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: None,
            base_manifest_etag: None,
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: tree,
            }],
            candidate_manifest: candidate,
            push_commit_receipt: None,
            staged_objects: vec![pack_object],
        };

        let err =
            compute_changed_paths(&store, &router, "org/repo", &plan, &plan.ref_updates, None)
                .await
                .expect_err("server receive must reject non-commit ref tips");

        assert!(
            err.to_string()
                .contains("new_oid must reference a commit object"),
            "unexpected error: {err}"
        );
        Ok(())
    }
}
