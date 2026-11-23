use super::*;
use crate::api::contents;
use crate::cells::repository::suggestions::{
    MAX_SUGGESTIONS, PrepareSuggestions, PrepareSuggestionsInput, PrepareSuggestionsOutcome,
    SuggestionSelection, SuggestionVersion, VerifySuggestionVersions,
};
use crate::git::objects::{commit_bytes, commit_identity};
use crate::git::receive::{self, ReceiveError, SuggestionPublication};
use crab_remote::objects::{TreeEdits, object_id};
use crab_remote_git::{ContentClassification, EntryKind, EntryMode, OperationKind, Revision};
use gix_hash::ObjectId;
use gix_object::{Kind, tree};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use uuid::Uuid;

const MAX_FILE_BYTES: usize = 900 * 1024;
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    request_id: Uuid,
    expected_head: String,
    threads: Vec<SuggestionVersion>,
    message: Option<String>,
}

struct Batch {
    number: u64,
    head: ObjectId,
    threads: Vec<SuggestionVersion>,
    message: String,
    plan_id: String,
}

pub(super) async fn apply(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> std::result::Result<Json<Value>, contents::Error> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let Json(input) = input?;
    apply_batch(server, principal, &repo, &repo, number, input).await
}

pub(super) async fn apply_to_fork(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> std::result::Result<Json<Value>, contents::Error> {
    let fork = app::repository(&server, &principal, &(owner, name))?;
    let upstream = fork.fork_source(&server).ok_or(Error::NotFound)?;
    let Json(input) = input?;
    // The fork's live membership inherits upstream grants. The credential
    // stays scoped to the repository whose ref this request can publish.
    apply_batch(server, principal, &upstream, &fork, number, input).await
}

async fn apply_batch(
    server: Arc<Server>,
    principal: Principal,
    discussion: &Repository,
    target: &Repository,
    number: u64,
    mut input: Input,
) -> std::result::Result<Json<Value>, contents::Error> {
    use contents::Error as WriteError;
    require_writer(&server, &principal, target).await?;
    if input.threads.is_empty() || input.threads.len() > MAX_SUGGESTIONS {
        return Err(WriteError::Input("Select 1–20 suggestions to commit"));
    }
    input.threads.sort_unstable_by_key(|thread| thread.number);
    if input
        .threads
        .windows(2)
        .any(|pair| pair[0].number == pair[1].number)
    {
        return Err(WriteError::Input("Select each suggestion only once"));
    }
    if input.request_id.is_nil() {
        return Err(WriteError::Input("Use a nonzero suggestion request ID"));
    }
    let number = app::number(number)?;
    let message = input.message.as_deref().map(str::trim).map_or_else(
        || format!("Apply review suggestions from pull request #{number}"),
        str::to_owned,
    );
    contents::validate_message(&message)?;
    let head = api::statuses::parse_oid(&input.expected_head)
        .map_err(|_| WriteError::Input("Use the exact head commit to apply suggestions"))?;
    let actor = app::actor(&principal)?;
    let digest = crab_metadata::receipts::publication_request_digest(&(
        "pull_request_suggestions",
        target.id,
        discussion.id,
        target.identity.placement_generation(),
        number,
        &actor.issuer,
        &actor.subject,
        head.to_string(),
        &input.threads,
        &message,
    ))
    .map_err(ReceiveError::from)?;
    let batch = Batch {
        number,
        head,
        threads: input.threads,
        message,
        plan_id: api::encode_hex(&crab_metadata::receipts::publication_plan_id(
            &digest,
            input.request_id.as_bytes(),
        )),
    };
    if let Some(commit) = recover_batch(&server, &principal, target, &batch).await? {
        return Ok(Json(json!({"commit":commit})));
    }
    let commit = match publish_batch(&server, &principal, discussion, target, actor, &batch).await {
        Ok(commit) => commit.to_string(),
        // Another attempt can commit after our first receipt read, including
        // before validation notices the head or selected conversations changed.
        Err(error) => recover_batch(&server, &principal, target, &batch)
            .await?
            .ok_or(error)?,
    };
    Ok(Json(json!({"commit":commit})))
}

async fn require_writer(
    server: &Server,
    principal: &Principal,
    target: &Repository,
) -> std::result::Result<(), contents::Error> {
    if !app::membership(server, principal, target)
        .await?
        .principal
        .can_write(target.id)
    {
        return Err(contents::Error::Permission);
    }
    Ok(())
}

async fn recover_batch(
    server: &Server,
    principal: &Principal,
    target: &Repository,
    batch: &Batch,
) -> std::result::Result<Option<String>, contents::Error> {
    let commit = super::super::git::committed_branch_edit(
        target,
        &batch.plan_id,
        None,
        Some(&batch.head.to_string()),
    )
    .await?;
    if commit.is_some() {
        require_writer(server, principal, target).await?;
    }
    Ok(commit)
}

async fn publish_batch(
    server: &Arc<Server>,
    principal: &Principal,
    discussion: &Repository,
    target: &Repository,
    actor: Identity,
    batch: &Batch,
) -> std::result::Result<ObjectId, contents::Error> {
    use contents::Error as WriteError;
    let pull = active_pull(server, discussion, target, batch.number)
        .await
        .map_err(|error| match error {
            Error::Archived => WriteError::from(ReceiveError::Archived),
            error => WriteError::from(error),
        })?;
    let source = comparison(server, discussion, &pull).await?;
    if !source.live || source.head_oid != batch.head.to_string() {
        return Err(WriteError::Conflict);
    }
    let Some(repository) = source.repository else {
        return Err(WriteError::Conflict);
    };
    let head = batch.head;
    for selected in &batch.threads {
        app::number(selected.number)?;
    }
    let cell = app::repository_cell(server, discussion)?;
    let selected = app::query_output(
        cell.client
            .query::<PrepareSuggestions>(
                &cell.target,
                None,
                PrepareSuggestionsInput {
                    selection: SuggestionSelection {
                        pull: pull.number,
                        threads: batch.threads.clone(),
                    },
                    base_oid: source.base_oid,
                    head_oid: source.head_oid,
                },
            )
            .await,
    )?;
    let selected = match selected {
        PrepareSuggestionsOutcome::Ready(selected) => selected,
        PrepareSuggestionsOutcome::Missing => {
            return Err(WriteError::Input("A selected suggestion no longer exists"));
        }
        PrepareSuggestionsOutcome::NotSuggestion => {
            return Err(WriteError::Input("Select review comments with suggestions"));
        }
        PrepareSuggestionsOutcome::Changed => return Err(WriteError::FileChanged),
    };
    let mut files: BTreeMap<Vec<u8>, Vec<Replacement>> = BTreeMap::new();
    let mut authors = BTreeMap::new();
    for thread in selected {
        if thread.author.issuer != actor.issuer || thread.author.subject != actor.subject {
            let (name, email) = commit_identity(&super::super::storage::from_author(thread.author));
            authors.entry(email).or_insert(name);
        }
        files.entry(thread.path).or_default().push(Replacement {
            start: usize::try_from(thread.start_line).map_err(|_| WriteError::FileChanged)?,
            end: usize::try_from(thread.end_line).map_err(|_| WriteError::FileChanged)?,
            blob: thread.blob,
            text: thread.text,
        });
    }
    let operation = repository
        .operation(OperationKind::Repository, &server.cancellation)
        .await?;
    let built = build(&repository, &operation, head, files).await;
    let (tree, mut objects) = match built {
        Err(WriteError::Remote(error)) => operation.finish(Err(error)).await?,
        built => {
            operation.finish(Ok(())).await?;
            built?
        }
    };
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let mut message = batch.message.clone();
    if !authors.is_empty() {
        message.push('\n');
        for (email, name) in authors {
            let _ = write!(message, "\nCo-authored-by: {name} <{email}>");
        }
    }
    let commit = commit_bytes(tree, &[head], &actor, &message, seconds)
        .map_err(|error| WriteError::Build(Box::new(error)))?;
    let oid = object_id(Kind::Commit, &commit)
        .map_err(|error| WriteError::Build(Box::new(error.into())))?;
    objects.push((Kind::Commit, commit));
    receive::publish_suggestion_objects(
        Arc::clone(server),
        principal.clone(),
        (target.config.owner.clone(), target.config.name.clone()),
        crab_git::receive_plan::RefUpdate {
            name: pull.head_ref,
            old: Some(head),
            new: Some(oid),
        },
        objects,
        SuggestionPublication {
            upstream: (target.id != discussion.id).then_some(discussion.id),
            number: batch.number,
            threads: batch.threads.clone(),
            plan_id: batch.plan_id.clone(),
        },
    )
    .await
    .map_err(|error| match error {
        ReceiveError::Coordination(crab_coordination::CoordinationError::PushLockHeld {
            ..
        }) => ReceiveError::Busy,
        error => error,
    })?;
    Ok(oid)
}

async fn active_pull(
    server: &Server,
    discussion: &Repository,
    target: &Repository,
    number: u64,
) -> Result<pull_storage::PullRequest> {
    let pull = load_pull(server, discussion, number).await?;
    if pull.head_repository.unwrap_or(discussion.id) != target.id {
        return Err(Error::NotFound);
    }
    if pull.state != pull_storage::PullState::Open || pull.merge_pending.is_some() {
        return Err(Error::Invalid("Suggestions require an open pull request"));
    }
    if crate::repository::settings::load_lifecycle(server, discussion)
        .await?
        .archived
    {
        return Err(Error::Archived);
    }
    Ok(pull)
}

pub(crate) async fn authorize_suggestion_publication(
    server: &Server,
    target: &Repository,
    suggestion: &SuggestionPublication,
    branch: &str,
) -> std::result::Result<(), ReceiveError> {
    let upstream = suggestion
        .upstream
        .map(|id| {
            target
                .fork_source(server)
                .filter(|entry| entry.id == id)
                .ok_or(ReceiveError::NotFound)
        })
        .transpose()?;
    let discussion = upstream.as_deref().unwrap_or(target);
    let pull = active_pull(server, discussion, target, suggestion.number)
        .await
        .map_err(|error| match error {
            Error::Invalid(_) => ReceiveError::BranchChanged,
            Error::Archived => ReceiveError::Archived,
            error => ReceiveError::from(error),
        })?;
    if pull.head_ref != branch {
        return Err(ReceiveError::BranchChanged);
    }
    // The native lease also covers the exact head. Recheck the selected
    // versions here so edits or a merge admitted during packing win first.
    let cell = app::repository_cell(server, discussion)?;
    let current = app::query_output(
        cell.client
            .query::<VerifySuggestionVersions>(
                &cell.target,
                None,
                SuggestionSelection {
                    pull: pull.number,
                    threads: suggestion.threads.clone(),
                },
            )
            .await,
    )?;
    if !current {
        return Err(ReceiveError::BranchChanged);
    }
    Ok(())
}

struct Replacement {
    start: usize,
    end: usize,
    blob: String,
    text: String,
}

type Objects = Vec<(Kind, Vec<u8>)>;

async fn build(
    repository: &RemoteGitRepository,
    operation: &crab_remote_git::OperationContext,
    head: ObjectId,
    files: BTreeMap<Vec<u8>, Vec<Replacement>>,
) -> std::result::Result<(ObjectId, Objects), contents::Error> {
    use contents::Error as WriteError;
    let snapshot = repository
        .snapshot(&Revision::Commit(head), operation)
        .await?;
    let mut edits = TreeEdits::default();
    let mut objects = Vec::new();
    let mut total = 0;
    for (path, replacements) in files {
        let path = GitPath::new(path)?;
        let blob = snapshot.read_blob(&path, operation).await?;
        if blob.metadata.kind != EntryKind::Blob
            || blob.metadata.classification != ContentClassification::OrdinaryGit
            || blob.bytes.contains(&0)
            || blob.bytes.len() > MAX_FILE_BYTES
        {
            return Err(WriteError::Input(
                "Suggestions require UTF-8 text files of at most 900 KiB",
            ));
        }
        let blob_oid = blob.metadata.oid.to_string();
        if replacements.iter().any(|edit| edit.blob != blob_oid) {
            return Err(WriteError::FileChanged);
        }
        let original = std::str::from_utf8(&blob.bytes)
            .map_err(|_| WriteError::Input("Suggestions require UTF-8 text files"))?;
        let updated = replace_lines(original, replacements)?;
        total += updated.len();
        if updated.len() > MAX_FILE_BYTES || total > MAX_BATCH_BYTES {
            return Err(WriteError::Input(
                "Suggestion edits can write up to 900 KiB per file and 4 MiB in total",
            ));
        }
        if updated == original {
            continue;
        }
        let bytes = updated.into_bytes();
        let oid = object_id(Kind::Blob, &bytes)
            .map_err(|error| WriteError::Build(Box::new(error.into())))?;
        let mode = if blob.metadata.mode == EntryMode::Executable {
            tree::EntryKind::BlobExecutable
        } else {
            tree::EntryKind::Blob
        };
        edits
            .insert(&path, Some((mode.into(), oid)))
            .map_err(|error| WriteError::Build(Box::new(error)))?;
        objects.push((Kind::Blob, bytes));
    }
    if objects.is_empty() {
        return Err(WriteError::Unchanged);
    }
    let tree = edits
        .apply(operation, snapshot.root_tree_oid(), &mut objects)
        .await
        .map_err(|error| WriteError::Build(Box::new(error)))?;
    Ok((tree, objects))
}

fn replace_lines(
    original: &str,
    mut replacements: Vec<Replacement>,
) -> std::result::Result<String, contents::Error> {
    let separator = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut offsets = vec![0];
    for line in original.split_inclusive('\n') {
        offsets.push(offsets.last().copied().unwrap_or_default() + line.len());
    }
    let mut next = 0;
    let mut cursor = 0;
    let mut output = String::with_capacity(original.len());
    replacements.sort_unstable_by_key(|edit| edit.start);
    for edit in &replacements {
        if edit.start <= next || edit.end < edit.start || edit.end >= offsets.len() {
            return Err(contents::Error::Input(
                "Selected suggestions have overlapping or invalid line ranges",
            ));
        }
        let start = offsets[edit.start - 1];
        let end = offsets[edit.end];
        output.push_str(&original[cursor..start]);
        if !edit.text.is_empty() {
            let text = edit.text.replace("\r\n", "\n").replace('\r', "\n");
            output.push_str(
                &text
                    .strip_suffix('\n')
                    .unwrap_or(&text)
                    .replace('\n', separator),
            );
            let replaced = &original[start..end];
            if replaced.ends_with("\r\n") {
                output.push_str("\r\n");
            } else if replaced.ends_with('\n') {
                output.push('\n');
            } else if text.ends_with('\n') {
                output.push_str(separator);
            }
        }
        cursor = end;
        next = edit.end;
    }
    output.push_str(&original[cursor..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: usize, end: usize, text: &str) -> Replacement {
        Replacement {
            start,
            end,
            blob: String::new(),
            text: text.into(),
        }
    }

    #[test]
    fn replacements_keep_untouched_bytes_and_use_original_line_ranges() {
        for (original, edits, expected) in [
            (
                "one\ntwo\nthree\n",
                vec![edit(2, 2, "TWO")],
                "one\nTWO\nthree\n",
            ),
            (
                "one\r\ntwo\r\n",
                vec![edit(2, 2, "TWO\n2")],
                "one\r\nTWO\r\n2\r\n",
            ),
            ("one\ntwo\nthree", vec![edit(2, 2, "")], "one\nthree"),
            ("one\ntwo\n", vec![edit(2, 2, "")], "one\n"),
            ("one\n", vec![edit(1, 1, "")], ""),
            ("one\ntwo", vec![edit(2, 2, "TWO\n")], "one\nTWO\n"),
            ("😀\ncafé", vec![edit(1, 1, "🐚")], "🐚\ncafé"),
            (
                "1\r\n2\n3\r\n4",
                vec![edit(4, 4, "four"), edit(1, 1, "A\nB")],
                "A\r\nB\r\n2\n3\r\nfour",
            ),
        ] {
            assert_eq!(replace_lines(original, edits).unwrap(), expected);
        }
    }

    #[test]
    fn invalid_and_overlapping_line_ranges_are_rejected() {
        for edits in [
            vec![edit(0, 1, "")],
            vec![edit(2, 1, "")],
            vec![edit(1, 3, "")],
            vec![edit(1, 2, ""), edit(2, 2, "")],
        ] {
            assert!(replace_lines("one\ntwo", edits).is_err());
        }
    }
}
