use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{FetchDepth, FetchOptions, FetchOutcome, local_error, transfer_error};
use crate::{Error, ErrorKind, RepositoryLocator, Result};

mod recovery;
use recovery::{
    apply_file_edit, read_fetch_intent, read_optional_utf8, remove_fetch_intent, write_fetch_intent,
};

const FETCH_INTENT: &str = "crab-sdk-fetch-intent-v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MetadataEdit {
    before: Option<String>,
    after: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FetchIntent {
    version: u8,
    refs: std::collections::BTreeMap<String, MetadataEdit>,
    remote_head_name: String,
    remote_head: MetadataEdit,
    shallow: Option<MetadataEdit>,
    fetch_head: MetadataEdit,
}

pub(super) async fn fetch_locked(
    state: &crate::client::ClientState,
    locator: Option<RepositoryLocator>,
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    options: &FetchOptions,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<FetchOutcome> {
    recover_fetch_intent(tools, root, git_dir, common_dir, cancel).await?;
    if let Some(locator) = locator {
        let resolved = state.resolve_repository(&locator, cancel).await?;
        let layout =
            crab_storage::StoreLayout::new(resolved.store.clone(), resolved.prefix.clone());
        let snapshot = crab_remote::transfer::fetch_snapshot(
            &resolved.store,
            &layout,
            &common_dir.join("objects").join("pack"),
            cancel,
        )
        .await
        .map_err(transfer_error)?;
        let intent =
            prepare_fetch_intent(tools, root, git_dir, common_dir, options, &snapshot, cancel)
                .await?;
        write_fetch_intent(git_dir, &intent).await?;
        apply_fetch_intent(tools, root, git_dir, common_dir, &intent, cancel).await?;
        verify_repository(tools, root, cancel).await?;
        remove_fetch_intent(git_dir).await?;
    } else {
        let environment = state
            .local_store
            .as_ref()
            .map(crate::DirectStoreOptions::local_environment)
            .transpose()?
            .unwrap_or_default();
        fetch_via_git(
            tools,
            root,
            &environment,
            options,
            state.local_execution_policy.is_trusted(),
            cancel,
        )
        .await?;
        verify_repository(tools, root, cancel).await?;
    }
    local_state(tools, root, cancel).await
}

pub(super) async fn recover_fetch_intent(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let path = git_dir.join(FETCH_INTENT);
    if !path.exists() {
        return Ok(());
    }
    let intent = read_fetch_intent(git_dir).await?;
    apply_fetch_intent(tools, root, git_dir, common_dir, &intent, cancel).await?;
    verify_repository(tools, root, cancel).await?;
    remove_fetch_intent(git_dir).await
}

pub(super) async fn validate_fetch_intent_on_open(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    git_dir: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    if !git_dir.join(FETCH_INTENT).exists() {
        return Ok(());
    }
    let intent = read_fetch_intent(git_dir).await?;
    validate_fetch_intent(tools, root, &intent, cancel).await
}

pub(super) async fn apply_fetch_state(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    options: &FetchOptions,
    snapshot: &crab_remote::transfer::FetchSnapshot,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let intent =
        prepare_fetch_intent(tools, root, git_dir, common_dir, options, snapshot, cancel).await?;
    apply_fetch_intent(tools, root, git_dir, common_dir, &intent, cancel).await
}

async fn prepare_fetch_intent(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    options: &FetchOptions,
    snapshot: &crab_remote::transfer::FetchSnapshot,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<FetchIntent> {
    let prefix = format!("refs/remotes/{}/", options.remote);
    let refs = tools
        .run_git(
            Some(root),
            [
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                prefix.as_str(),
                "refs/tags/",
            ],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let mut current = parse_local_refs(&refs.stdout)?;
    current.remove(&format!("{prefix}HEAD"));
    let mut desired = std::collections::BTreeMap::new();
    for (name, target) in &snapshot.refs {
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            desired.insert(format!("{prefix}{branch}"), target.clone());
        } else if name.starts_with("refs/tags/") && options.tags != Some(false) {
            desired.insert(name.clone(), target.clone());
        }
    }

    let mut edits = std::collections::BTreeMap::new();
    for (name, target) in &desired {
        match current.get(name) {
            Some(old) if old == target => {
                edits.insert(
                    name.clone(),
                    MetadataEdit {
                        before: Some(old.clone()),
                        after: Some(target.clone()),
                    },
                );
            }
            Some(_) if name.starts_with("refs/tags/") => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "fetch would replace an existing local tag",
                ));
            }
            Some(old) => {
                edits.insert(
                    name.clone(),
                    MetadataEdit {
                        before: Some(old.clone()),
                        after: Some(target.clone()),
                    },
                );
            }
            None => {
                edits.insert(
                    name.clone(),
                    MetadataEdit {
                        before: None,
                        after: Some(target.clone()),
                    },
                );
            }
        }
    }
    if options.prune {
        for (name, old) in &current {
            let prune_tag = options.tags == Some(true) && name.starts_with("refs/tags/");
            if (name.starts_with(&prefix) || prune_tag) && !desired.contains_key(name) {
                edits.insert(
                    name.clone(),
                    MetadataEdit {
                        before: Some(old.clone()),
                        after: None,
                    },
                );
            }
        }
    }

    let remote_head_name = format!("{prefix}HEAD");
    let remote_head_before = symbolic_ref(tools, root, &remote_head_name, cancel).await?;
    let remote_head_after = if let Some(head) = snapshot.head.strip_prefix("refs/heads/")
        && snapshot.refs.contains_key(&snapshot.head)
    {
        Some(format!("{prefix}{head}"))
    } else {
        remote_head_before.clone()
    };
    let shallow = prepare_shallow_edit(
        tools,
        root,
        common_dir,
        options.depth,
        &prefix,
        &desired,
        cancel,
    )
    .await?;
    let fetch_head_before = read_optional_utf8(&git_dir.join("FETCH_HEAD"), "FETCH_HEAD").await?;
    let fetch_head_after = build_fetch_head(tools, root, options, snapshot, cancel).await?;
    Ok(FetchIntent {
        version: 1,
        refs: edits,
        remote_head_name,
        remote_head: MetadataEdit {
            before: remote_head_before,
            after: remote_head_after,
        },
        shallow,
        fetch_head: MetadataEdit {
            before: fetch_head_before,
            after: Some(fetch_head_after),
        },
    })
}

async fn apply_fetch_intent(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    intent: &FetchIntent,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    validate_fetch_intent(tools, root, intent, cancel).await?;
    let refs = tools
        .run_git(
            Some(root),
            ["for-each-ref", "--format=%(refname) %(objectname)"],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let current = parse_local_refs(&refs.stdout)?;
    let mut transaction = Vec::new();
    for (name, edit) in &intent.refs {
        let value = current.get(name).cloned();
        if value == edit.after {
            continue;
        }
        if value != edit.before {
            return Err(Error::new(
                ErrorKind::Conflict,
                "local ref changed while recovering fetch metadata",
            ));
        }
        match (&edit.before, &edit.after) {
            (None, Some(after)) => {
                transaction.extend_from_slice(format!("create {name} {after}\n").as_bytes());
            }
            (Some(before), Some(after)) => {
                transaction
                    .extend_from_slice(format!("update {name} {after} {before}\n").as_bytes());
            }
            (Some(before), None) => {
                transaction.extend_from_slice(format!("delete {name} {before}\n").as_bytes());
            }
            (None, None) => {}
        }
    }
    if !transaction.is_empty() {
        tools
            .run_git_with_input(
                Some(root),
                ["update-ref", "--stdin"],
                transaction,
                false,
                cancel,
            )
            .await
            .map_err(local_error)?;
    }

    apply_symbolic_ref_edit(
        tools,
        root,
        &intent.remote_head_name,
        &intent.remote_head,
        cancel,
    )
    .await?;

    if let Some(edit) = &intent.shallow {
        apply_file_edit(&common_dir.join("shallow"), edit, "shallow boundary").await?;
    }
    apply_file_edit(
        &git_dir.join("FETCH_HEAD"),
        &intent.fetch_head,
        "FETCH_HEAD",
    )
    .await
}

async fn validate_fetch_intent(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    intent: &FetchIntent,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    if intent.version != 1 {
        return Err(Error::new(
            ErrorKind::Corruption,
            "SDK fetch intent has an unsupported version",
        ));
    }
    tools
        .run_git(
            Some(root),
            ["check-ref-format", intent.remote_head_name.as_str()],
            false,
            cancel,
        )
        .await
        .map_err(|_| {
            Error::new(
                ErrorKind::Corruption,
                "SDK fetch intent has an invalid symbolic ref",
            )
        })?;
    let mut required_objects = std::collections::BTreeSet::new();
    for (name, edit) in &intent.refs {
        tools
            .run_git(
                Some(root),
                ["check-ref-format", name.as_str()],
                false,
                cancel,
            )
            .await
            .map_err(|_| {
                Error::new(ErrorKind::Corruption, "SDK fetch intent has an invalid ref")
            })?;
        for oid in [&edit.before, &edit.after].into_iter().flatten() {
            crate::ObjectId::from_hex(oid).map_err(|_| {
                Error::new(
                    ErrorKind::Corruption,
                    "SDK fetch intent has an invalid object ID",
                )
            })?;
        }
        if let Some(after) = &edit.after {
            required_objects.insert(after);
        }
    }
    for target in [&intent.remote_head.before, &intent.remote_head.after]
        .into_iter()
        .flatten()
    {
        tools
            .run_git(
                Some(root),
                ["check-ref-format", target.as_str()],
                false,
                cancel,
            )
            .await
            .map_err(|_| {
                Error::new(
                    ErrorKind::Corruption,
                    "SDK fetch intent has an invalid symbolic ref target",
                )
            })?;
    }
    if !required_objects.is_empty() {
        // Prove every destination before the ref transaction. The fsck after
        // recovery cannot undo a ref that already exposed a missing object.
        let mut input = Vec::with_capacity(required_objects.len() * 41);
        let mut expected = Vec::with_capacity(required_objects.len() * 41);
        for oid in required_objects {
            input.extend_from_slice(oid.as_bytes());
            input.push(b'\n');
            expected.extend_from_slice(oid.as_bytes());
            expected.push(b'\n');
        }
        let output = tools
            .run_git_with_input(
                Some(root),
                ["cat-file", "--batch-check=%(objectname)"],
                input,
                false,
                cancel,
            )
            .await
            .map_err(local_error)?;
        if output.stdout != expected {
            return Err(Error::new(
                ErrorKind::Corruption,
                "SDK fetch intent references a missing Git object",
            ));
        }
    }
    Ok(())
}

async fn symbolic_ref(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    name: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<String>> {
    let output = tools
        .run_git(
            Some(root),
            ["for-each-ref", "--format=%(symref)", name],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let value = std::str::from_utf8(&output.stdout)
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Corruption,
                "Git symbolic ref is not UTF-8",
                source,
            )
        })?
        .trim();
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

async fn apply_symbolic_ref_edit(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    name: &str,
    edit: &MetadataEdit,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let current = symbolic_ref(tools, root, name, cancel).await?;
    if current == edit.after {
        return Ok(());
    }
    if current != edit.before {
        return Err(Error::new(
            ErrorKind::Conflict,
            "local symbolic ref changed while recovering fetch metadata",
        ));
    }
    let args = match &edit.after {
        Some(after) => vec!["symbolic-ref", name, after.as_str()],
        None => vec!["symbolic-ref", "--delete", name],
    };
    tools
        .run_git(Some(root), args, false, cancel)
        .await
        .map(drop)
        .map_err(local_error)
}

async fn fetch_via_git(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    environment: &[(OsString, OsString)],
    options: &FetchOptions,
    trusted: bool,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let mut args: Vec<OsString> = vec!["fetch".into(), options.remote.clone().into()];
    if options.prune {
        args.push("--prune".into());
        if options.tags == Some(true) {
            args.push("--prune-tags".into());
        }
    }
    match options.tags {
        Some(true) => args.push("--tags".into()),
        Some(false) => args.push("--no-tags".into()),
        None => {}
    }
    match options.depth {
        FetchDepth::Preserve => {}
        FetchDepth::Depth(depth) => {
            args.extend(["--depth".into(), depth.to_string().into()]);
        }
        FetchDepth::Deepen(depth) => {
            args.extend(["--deepen".into(), depth.to_string().into()]);
        }
        FetchDepth::Unshallow => args.push("--unshallow".into()),
    }
    tools
        .run_git_with_env(
            Some(root),
            args,
            environment.iter().cloned(),
            trusted,
            cancel,
        )
        .await
        .map(drop)
        .map_err(local_error)
}

fn parse_local_refs(bytes: &[u8]) -> Result<std::collections::BTreeMap<String, String>> {
    let text = std::str::from_utf8(bytes).map_err(|source| {
        Error::with_source(
            ErrorKind::Corruption,
            "Git ref inventory is not UTF-8",
            source,
        )
    })?;
    let mut refs = std::collections::BTreeMap::new();
    for line in text.lines() {
        let (name, target) = line.split_once(' ').ok_or_else(|| {
            Error::new(
                ErrorKind::Corruption,
                "Git returned a malformed ref inventory",
            )
        })?;
        crate::ObjectId::from_hex(target)?;
        refs.insert(name.to_owned(), target.to_owned());
    }
    Ok(refs)
}

async fn prepare_shallow_edit(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    common_dir: &Path,
    depth: FetchDepth,
    remote_prefix: &str,
    refs: &std::collections::BTreeMap<String, String>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<MetadataEdit>> {
    let shallow = common_dir.join("shallow");
    let before = read_optional_utf8(&shallow, "shallow boundary").await?;
    let boundaries = match depth {
        FetchDepth::Preserve => return Ok(None),
        FetchDepth::Unshallow => std::collections::BTreeSet::new(),
        FetchDepth::Depth(depth) => {
            let tips = refs
                .iter()
                .filter(|(name, _)| name.starts_with(remote_prefix))
                .map(|(_, target)| target.clone())
                .collect();
            shallow_boundaries(tools, root, tips, depth.saturating_sub(1), cancel).await?
        }
        FetchDepth::Deepen(depth) => {
            let current = match tokio::fs::read_to_string(&shallow).await {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(source) => {
                    return Err(Error::with_source(
                        ErrorKind::Io,
                        "cannot read local shallow boundary",
                        source,
                    ));
                }
            };
            let tips = current
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            shallow_boundaries(tools, root, tips, depth, cancel).await?
        }
    };
    let after = if boundaries.is_empty() {
        None
    } else {
        let mut body = boundaries.into_iter().collect::<Vec<_>>().join("\n");
        body.push('\n');
        Some(body)
    };
    Ok(Some(MetadataEdit { before, after }))
}

async fn shallow_boundaries(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    tips: Vec<String>,
    skip: u32,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<std::collections::BTreeSet<String>> {
    let mut boundaries = tips.into_iter().collect::<std::collections::BTreeSet<_>>();
    let mut visited = 0usize;
    for _ in 0..skip {
        let mut parents = std::collections::BTreeSet::new();
        for commit in &boundaries {
            visited = visited.saturating_add(1);
            if visited > 1_000_000 {
                return Err(Error::new(
                    ErrorKind::LimitExceeded,
                    "shallow boundary traversal exceeds 1000000 commits",
                ));
            }
            let output = tools
                .run_git(
                    Some(root),
                    ["cat-file", "-p", commit.as_str()],
                    false,
                    cancel,
                )
                .await
                .map_err(local_error)?;
            let body = std::str::from_utf8(&output.stdout).map_err(|source| {
                Error::with_source(
                    ErrorKind::Corruption,
                    "Git commit object is not UTF-8",
                    source,
                )
            })?;
            for line in body.lines().take_while(|line| !line.is_empty()) {
                if let Some(parent) = line.strip_prefix("parent ") {
                    crate::ObjectId::from_hex(parent)?;
                    parents.insert(parent.to_owned());
                }
            }
        }
        boundaries = parents;
        if boundaries.is_empty() {
            break;
        }
    }
    Ok(boundaries)
}

async fn build_fetch_head(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    options: &FetchOptions,
    snapshot: &crab_remote::transfer::FetchSnapshot,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    let remote = tools
        .run_git(
            Some(root),
            ["remote", "get-url", options.remote.as_str()],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let remote = std::str::from_utf8(&remote.stdout)
        .map_err(|source| {
            Error::with_source(ErrorKind::Corruption, "Git remote URL is not UTF-8", source)
        })?
        .trim();
    let mut body = String::new();
    for (name, target) in &snapshot.refs {
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            body.push_str(&format!("{target}\t\tbranch '{branch}' of {remote}\n"));
        }
    }
    Ok(body)
}

pub(super) async fn verify_repository(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    tools
        .run_git(
            Some(root),
            ["fsck", "--connectivity-only", "--no-dangling"],
            false,
            cancel,
        )
        .await
        .map(drop)
        .map_err(local_error)
}

async fn local_state(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<FetchOutcome> {
    let head = tools
        .run_git(Some(root), ["rev-parse", "--verify", "HEAD"], false, cancel)
        .await
        .ok()
        .and_then(|output| std::str::from_utf8(&output.stdout).ok()?.trim().parse_oid());
    let shallow = tools
        .run_git(
            Some(root),
            ["rev-parse", "--is-shallow-repository"],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?
        .stdout
        .starts_with(b"true");
    Ok(FetchOutcome { head, shallow })
}

trait ParseOid {
    fn parse_oid(&self) -> Option<crate::ObjectId>;
}

impl ParseOid for str {
    fn parse_oid(&self) -> Option<crate::ObjectId> {
        crate::ObjectId::from_hex(self).ok()
    }
}
