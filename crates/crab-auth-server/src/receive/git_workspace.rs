//! Git verification and source-commit synthesis for Capsule receive.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crab_auth::{PushRefUpdate, normalize_optional_oid};
use crab_metadata::{
    capsule_protocol::{
        Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleTransaction, CapsuleVisibilityDelta,
    },
    git_visibility::GitVisibilityEdit,
};
use crab_storage::{Store, StoreLayout};

use crate::error::{AuthServerError, Result};
use crate::git_pointer_scan::{ReachablePointerScan, scan_reachable_pointers_from_refs};

use super::{derive_peeled_refs, invalid, validate_sha1};

pub(super) async fn verify_capsule_git_candidate(
    router: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    candidate: &Capsule,
    ref_updates: &[PushRefUpdate],
    max_input_bytes: u64,
) -> Result<(Vec<String>, ReachablePointerScan)> {
    let temp = tempfile::tempdir()?;
    let git_dir = temp.path().join("source.git");
    run_git(["init", "--bare", path_str(&git_dir)?], None)?;
    crab_read::capsule_protocol::install_git_packs_with_candidates_from_store(
        view,
        std::slice::from_ref(candidate),
        router,
        &git_dir,
        max_input_bytes,
        None,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    let paths = compute_changed_paths_in(&git_dir, ref_updates, &BTreeMap::new(), false)?;

    let refs = verify_capsule_candidate_refs(&git_dir, view, candidate).await?;
    let pointers = scan_reachable_pointers_from_refs(&git_dir, &refs)?;
    Ok((paths, pointers))
}

async fn verify_capsule_candidate_refs(
    git_dir: &Path,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    candidate: &Capsule,
) -> Result<Vec<(String, String)>> {
    let transaction = candidate.transaction()?;
    let mut refs = view.refs().clone();
    let mut peeled = view.peeled_refs().clone();
    for edit in transaction.edits() {
        match edit.new_oid() {
            Some(oid) => {
                refs.insert(edit.ref_name().to_owned(), oid.to_owned());
            }
            None => {
                refs.remove(edit.ref_name());
            }
        }
        match edit.peeled_oid() {
            Some(oid) => {
                peeled.insert(edit.ref_name().to_owned(), oid.to_owned());
            }
            None => {
                peeled.remove(edit.ref_name());
            }
        }
    }
    let refs = refs.into_iter().collect::<Vec<_>>();
    let reachable = crab_git::walk_reachable_by_ref_bounded(
        git_dir,
        &refs,
        &peeled,
        usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
            .map_err(|_| invalid("Git visibility object limit does not fit usize"))?,
    )
    .map_err(|source| AuthServerError::GitVisibilityWalk { source })?;
    // Pack presence is not read authority. Reintroducing an invisible object
    // requires its body in the verified candidate, even if a retained base pack
    // happens to contain it.
    {
        let visible = view.git_visibility_index().await?;
        let mut allowed = visible
            .objects_for_refs(view.refs().keys().map(String::as_str))
            .into_iter()
            .collect::<HashSet<_>>();
        for pack in candidate.git_packs() {
            let index = candidate.section_bytes(pack.index_section())?;
            let (oids, _) = crab_git::pack_locator::sorted_object_ids_from_index_bytes(&index)
                .map_err(std::io::Error::other)?;
            for oid in oids {
                let oid = <[u8; 20]>::try_from(oid.as_bytes()).map_err(std::io::Error::other)?;
                allowed.insert(oid);
            }
        }
        if reachable.values().any(|objects| {
            objects
                .commits
                .iter()
                .chain(&objects.trees)
                .chain(&objects.blobs)
                .chain(&objects.tags)
                .any(|oid| !allowed.contains(oid))
        }) {
            return Err(invalid(
                "candidate Git object is neither visible in the captured view nor supplied by its pack",
            ));
        }
    }
    let closures = reachable
        .into_iter()
        .map(|(name, objects)| (name, reachable_object_ids(&objects)))
        .collect::<BTreeMap<_, _>>();
    let declared = view.candidate_git_visibility(candidate).await?;
    if closures != declared {
        return Err(invalid(
            "candidate capsule Git visibility differs from its reachable object closure",
        ));
    }
    Ok(refs)
}

pub(super) struct MaterializedCapsuleSourcePush {
    pub transaction: CapsuleTransaction,
    pub git_packs: Vec<CapsuleGitPack>,
    pub visibility: CapsuleVisibilityDelta,
    pub pointers: ReachablePointerScan,
    pub changed_paths: Vec<String>,
}

pub(super) async fn materialize_capsule_source_push(
    source_router: &StoreLayout<Store>,
    source_view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    filtered_view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    filtered_router: &StoreLayout<Store>,
    candidate: &Capsule,
    view_updates: &[PushRefUpdate],
    source_updates: &[PushRefUpdate],
    max_input_bytes: u64,
) -> Result<MaterializedCapsuleSourcePush> {
    if view_updates.len() != source_updates.len() {
        return Err(invalid("source ref updates do not match view ref updates"));
    }
    let temp = tempfile::tempdir()?;
    let git_dir = temp.path().join("source.git");
    run_git(["init", "--bare", path_str(&git_dir)?], None)?;
    crab_read::capsule_protocol::install_git_packs_with_candidates_from_store(
        filtered_view,
        std::slice::from_ref(candidate),
        filtered_router,
        &git_dir,
        max_input_bytes,
        None,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await?;

    let changed_paths =
        compute_changed_paths_in(&git_dir, view_updates, filtered_view.refs(), true)?;
    // Prove the candidate using only filtered and incoming objects before the
    // private source ODB becomes available for source-commit synthesis.
    let _ = verify_capsule_candidate_refs(&git_dir, filtered_view, candidate).await?;
    crab_read::capsule_protocol::install_git_packs_with_candidates_from_store(
        source_view,
        &[],
        source_router,
        &git_dir,
        max_input_bytes,
        None,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    let mut final_updates = Vec::with_capacity(source_updates.len());
    let mut synthesized_tips = Vec::with_capacity(source_updates.len());
    for (view_update, source_update) in view_updates.iter().zip(source_updates) {
        let source_new = synthesize_source_commit(
            &git_dir,
            temp.path(),
            source_update.old_oid.as_deref(),
            view_update.old_oid.as_deref(),
            &view_update.new_oid,
        )?;
        synthesized_tips.push((source_new.clone(), source_update.old_oid.clone()));
        final_updates.push(PushRefUpdate {
            ref_name: source_update.ref_name.clone(),
            old_oid: source_update.old_oid.clone(),
            new_oid: source_new,
        });
    }
    validate_git_publication(&git_dir, &final_updates)?;
    let git_pack = build_synthesized_capsule_pack(&git_dir, temp.path(), &synthesized_tips)?;

    let mut final_refs = source_view.refs().clone();
    for update in &final_updates {
        final_refs.insert(update.ref_name.clone(), update.new_oid.clone());
    }
    let ref_pairs = final_refs.into_iter().collect::<Vec<_>>();
    let peeled_refs = derive_peeled_refs(&git_dir, &ref_pairs)?;
    let closures = crab_git::walk_reachable_by_ref_bounded(
        &git_dir,
        &ref_pairs,
        &peeled_refs,
        usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
            .map_err(|_| invalid("Git visibility object limit does not fit usize"))?,
    )
    .map_err(|source| AuthServerError::GitVisibilityWalk { source })?;
    let visibility = CapsuleVisibilityDelta::new(
        final_updates
            .iter()
            .map(|update| {
                let reachable = closures.get(&update.ref_name).ok_or_else(|| {
                    invalid(format!(
                        "source visibility omitted updated ref {}",
                        update.ref_name
                    ))
                })?;
                Ok((
                    update.ref_name.clone(),
                    GitVisibilityEdit::from_replacement_objects(
                        update.old_oid.clone(),
                        update.new_oid.clone(),
                        reachable_object_ids(reachable),
                    ),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?,
    )?;
    let transaction = CapsuleTransaction::for_protected_source(
        source_view.root().digest(),
        candidate.transaction_id(),
        final_updates
            .iter()
            .map(|update| {
                CapsuleRefEdit::new(
                    update.ref_name.clone(),
                    update.old_oid.clone(),
                    Some(update.new_oid.clone()),
                    peeled_refs.get(&update.ref_name).cloned(),
                )
            })
            .collect(),
    )?;
    let pointers = scan_reachable_pointers_from_refs(&git_dir, &ref_pairs)?;
    Ok(MaterializedCapsuleSourcePush {
        transaction,
        git_packs: vec![git_pack],
        visibility,
        pointers,
        changed_paths,
    })
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

fn synthesize_source_commit(
    git_dir: &Path,
    temp_root: &Path,
    source_old: Option<&str>,
    view_old: Option<&str>,
    view_new: &str,
) -> Result<String> {
    require_commit_object(git_dir, view_new, "new_oid")?;
    if let Some(old) = view_old {
        require_commit_object(git_dir, old, "old_oid")?;
    }
    if let Some(old) = source_old {
        require_commit_object(git_dir, old, "source old_oid")?;
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
    for path in ref_tree_changed_paths(git_dir, &update)? {
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
    let message_path = temp_root.join(format!("synth-{}.message", blake3_hex(view_new.as_bytes())));
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

fn compute_changed_paths_in(
    git_dir: &Path,
    ref_updates: &[PushRefUpdate],
    view_old_refs: &BTreeMap<String, String>,
    allow_equivalent_old_tree: bool,
) -> Result<Vec<String>> {
    for update in ref_updates {
        let update = effective_view_ref_update(update, view_old_refs);
        validate_ref_graph(git_dir, &update, allow_equivalent_old_tree)?;
    }
    validate_git_publication(git_dir, ref_updates)?;
    let mut paths = BTreeSet::new();
    for update in ref_updates {
        let update = effective_view_ref_update(update, view_old_refs);
        paths.extend(ref_tree_changed_paths(git_dir, &update)?);
        for commit in introduced_commits(git_dir, &update)? {
            paths.extend(commit_changed_paths(git_dir, &commit)?);
        }
    }
    Ok(paths.into_iter().collect())
}

fn validate_ref_graph(
    git_dir: &Path,
    update: &PushRefUpdate,
    allow_equivalent_old_tree: bool,
) -> Result<()> {
    require_commit_object(git_dir, &update.new_oid, "new_oid")?;
    let Some(old) = normalize_optional_oid(update.old_oid.as_deref()) else {
        return Ok(());
    };
    require_commit_object(git_dir, old.as_str(), "old_oid")?;
    match require_fast_forward(git_dir, update, old.as_str()) {
        Ok(()) => Ok(()),
        Err(AuthServerError::NonFastForward { .. })
            if allow_equivalent_old_tree
                && history_contains_tree(git_dir, &update.new_oid, old.as_str())? =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn require_commit_object(git_dir: &Path, oid: &str, field: &str) -> Result<()> {
    let output = run_git_capture(["--git-dir", path_str(git_dir)?, "cat-file", "-t", oid])?;
    if output.trim() != "commit" {
        return Err(invalid(format!("{field} must reference a commit object")));
    }
    Ok(())
}

fn require_fast_forward(git_dir: &Path, update: &PushRefUpdate, old_oid: &str) -> Result<()> {
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

fn history_contains_tree(git_dir: &Path, new_oid: &str, old_oid: &str) -> Result<bool> {
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

fn ref_tree_changed_paths(git_dir: &Path, update: &PushRefUpdate) -> Result<Vec<String>> {
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

fn introduced_commits(git_dir: &Path, update: &PushRefUpdate) -> Result<Vec<String>> {
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

fn commit_changed_paths(git_dir: &Path, commit: &str) -> Result<Vec<String>> {
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

fn effective_view_ref_update(
    update: &PushRefUpdate,
    view_old_refs: &BTreeMap<String, String>,
) -> PushRefUpdate {
    PushRefUpdate {
        ref_name: update.ref_name.clone(),
        old_oid: effective_view_old_oid(update, view_old_refs),
        new_oid: update.new_oid.clone(),
    }
}

fn effective_view_old_oid(
    update: &PushRefUpdate,
    view_old_refs: &BTreeMap<String, String>,
) -> Option<String> {
    view_old_refs
        .get(&update.ref_name)
        .cloned()
        .or_else(|| normalize_optional_oid(update.old_oid.as_deref()))
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

fn reachable_object_ids(reachable: &crab_git::walk::ReachableSet) -> Vec<String> {
    let mut objects = reachable
        .commits
        .iter()
        .chain(&reachable.trees)
        .chain(&reachable.blobs)
        .chain(&reachable.tags)
        .map(|oid| {
            let mut encoded = String::with_capacity(40);
            for byte in oid {
                use std::fmt::Write as _;
                let _ = write!(encoded, "{byte:02x}");
            }
            encoded
        })
        .collect::<Vec<_>>();
    objects.sort_unstable();
    objects.dedup();
    objects
}

fn build_synthesized_capsule_pack(
    git_dir: &Path,
    temp_root: &Path,
    tips: &[(String, Option<String>)],
) -> Result<CapsuleGitPack> {
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
    let pack_path = temp_root.join(format!("capsule-pack-{pack_id}.pack"));
    fs::write(&pack_path, &pack_bytes)?;
    run_git(
        ["index-pack", "--strict", path_str(&pack_path)?],
        Some(git_dir),
    )?;
    let idx_path = pack_path.with_extension("idx");
    let rev_path = pack_path.with_extension("rev");
    crab_git::pack_locator::write_pack_reverse_index(&idx_path, &rev_path)
        .map_err(crab_git::pack::PackError::from)?;
    let mut locations = crab_git::pack_locator::PackLocationIter::open(
        &idx_path,
        &rev_path,
        pack_bytes.len() as u64,
    )
    .map_err(crab_git::pack::PackError::from)?;
    let object_count = locations.object_count();
    let object_ids = locations
        .by_ref()
        .map(|location| location.map(|location| location.oid))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(crab_git::pack::PackError::from)?;
    let kinds =
        crab_git::object_kinds_from_git_dir(git_dir, &object_ids).map_err(AuthServerError::from)?;
    let ordered_kinds = object_ids
        .iter()
        .map(|oid| {
            kinds
                .get(oid)
                .copied()
                .ok_or_else(|| invalid("synthesized capsule pack kind metadata omitted an object"))
        })
        .collect::<Result<Vec<_>>>()?;
    let checksum = locations.pack_checksum();
    let locator = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds)
        .map_err(crab_git::pack::PackError::from)?;
    CapsuleGitPack::new(
        bytes::Bytes::from(pack_bytes),
        bytes::Bytes::from(fs::read(idx_path)?),
        bytes::Bytes::from(fs::read(rev_path)?),
        bytes::Bytes::from(locator),
        checksum.to_string(),
        object_count,
    )
    .map_err(Into::into)
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
}
