use std::collections::{HashMap, HashSet};

use crab_remote_git::{
    OperationContext, OperationKind, RemoteGitRepository, RemoteGitSnapshot, Revision,
    RevisionError,
};

use super::*;
use crate::cells::repository::{ListMergedPullsForRelease, MergedPullForRelease};

const MAX_MERGED_PULLS: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    target_oid: String,
    previous_tag: Option<String>,
}

#[derive(Serialize)]
pub(super) struct GeneratedNotes {
    target_oid: String,
    previous_tag: Option<String>,
    previous_oid: Option<String>,
    body: String,
}

pub(super) async fn generate(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(parameters): Query<Parameters>,
) -> Result<Json<GeneratedNotes>> {
    let repo = app::repository(&server, &principal, &key)?;
    if !principal.can_write(repo.id) {
        return Err(Error::ReleasePermission);
    }
    let target_oid = target_oid(&parameters.target_oid)?;
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let git = repo.revalidate(&server, &cancellation).await?.git;
    let operation = git
        .operation(OperationKind::History, &cancellation)
        .await
        .map_err(crate::Error::from)?;
    let result = draft(
        &server,
        &repo,
        &git,
        &operation,
        target_oid,
        parameters.previous_tag,
    )
    .await;
    // Finish with the Git failure itself so locator-close and interruption
    // errors keep the same precedence as they do in ordinary history reads.
    let git_result = match result {
        Err(Error::Repository(crate::Error::Remote(error))) => Err(error),
        response => Ok(response),
    };
    let notes = operation
        .finish(git_result)
        .await
        .map_err(crate::Error::from)??;
    let membership = app::membership(&server, &principal, &repo).await?;
    if !membership.principal.can_read(repo.id) {
        return Err(Error::NotFound);
    }
    if !membership.principal.can_write(repo.id) {
        return Err(Error::ReleasePermission);
    }
    Ok(Json(notes))
}

async fn draft(
    server: &Server,
    repo: &Repository,
    git: &RemoteGitRepository,
    operation: &OperationContext,
    target_oid: ObjectId,
    previous_tag: Option<String>,
) -> Result<GeneratedNotes> {
    let target = git
        .snapshot(&Revision::Commit(target_oid), operation)
        .await
        .map_err(|error| match error {
            crab_remote_git::Error::Revision {
                reason: RevisionError::NotFound | RevisionError::NotReachable,
            } => Error::CommitNotFound,
            error => crate::Error::from(error).into(),
        })?;
    let previous = match previous_tag.as_deref() {
        Some("") => None,
        Some(name) => {
            let (name, reference) = tag_name(name)?;
            let entry = git
                .refs()
                .find(&reference)
                .ok_or(Error::Invalid("The selected previous tag no longer exists"))?;
            let previous = entry.peeled.unwrap_or(entry.target);
            Some((name, previous))
        }
        None => published_predecessor(server, repo, git, &target, operation).await?,
    };
    let pulls = merged_pulls(server, repo).await?;
    let mut candidates = pulls
        .iter()
        .map(|pull| target_oid_from_merge(&pull.commit_oid))
        .collect::<Result<Vec<_>>>()?;
    candidates.extend(previous.as_ref().map(|(_, oid)| *oid));
    let mut included = target
        .commit_membership(&candidates, operation)
        .await
        .map_err(crate::Error::from)?;
    if let Some((_, previous_oid)) = &previous {
        if included.pop() != Some(true) {
            return Err(Error::Invalid(
                "The previous tag must point to the release target or one of its ancestors",
            ));
        }
        let base = git
            .snapshot(&Revision::Commit(*previous_oid), operation)
            .await
            .map_err(crate::Error::from)?;
        candidates.pop();
        let excluded = base
            .commit_membership(&candidates, operation)
            .await
            .map_err(crate::Error::from)?;
        for (included, excluded) in included.iter_mut().zip(excluded) {
            *included &= !excluded;
        }
    }
    let body = markdown(
        repo,
        target_oid,
        previous.as_ref().map(|(_, oid)| *oid),
        pulls
            .into_iter()
            .zip(included)
            .filter_map(|(pull, include)| include.then_some(pull)),
    )?;
    Ok(GeneratedNotes {
        target_oid: target_oid.to_string(),
        previous_oid: previous.as_ref().map(|(_, oid)| oid.to_string()),
        previous_tag: previous.map(|(name, _)| name),
        body,
    })
}

fn target_oid_from_merge(value: &str) -> Result<ObjectId> {
    target_oid(value).map_err(|_| {
        Error::Cell(crate::cells::Error::Invalid(
            "Completed merge has an invalid commit ID",
        ))
    })
}

async fn published_predecessor(
    server: &Server,
    repo: &Repository,
    git: &RemoteGitRepository,
    target: &RemoteGitSnapshot,
    operation: &OperationContext,
) -> Result<Option<(String, ObjectId)>> {
    let tags = git
        .refs()
        .entries
        .iter()
        .filter_map(|entry| {
            let oid = entry.peeled.unwrap_or(entry.target);
            let name = entry.name.strip_prefix("refs/tags/")?;
            (oid != target.commit_oid()).then_some((name, entry.target, oid))
        })
        .collect::<Vec<_>>();
    let candidates = tags.iter().map(|(_, _, oid)| *oid).collect::<Vec<_>>();
    let included = target
        .commit_membership(&candidates, operation)
        .await
        .map_err(crate::Error::from)?;
    let tags = tags
        .into_iter()
        .zip(included)
        .filter_map(|((name, tag, oid), include)| include.then_some((name, (tag, oid))))
        .collect::<HashMap<_, _>>();

    if tags.is_empty() {
        return Ok(None);
    }
    let mut before = None;
    loop {
        let (page, next) = storage::list(server, repo, before, 20, false, None).await?;
        for release in page {
            // A published release is eligible only while its tag still matches
            // the recorded original object and target in this Git capture.
            if let Some((tag, commit)) = tags.get(release.tag_name.as_str())
                && release.tag_oid.as_deref() == Some(tag.to_string().as_str())
                && release.target_oid == commit.to_string()
            {
                return Ok(Some((release.tag_name, *commit)));
            }
        }
        let Some(next) = next else { break };
        before = Some(next);
    }
    Ok(None)
}

async fn merged_pulls(server: &Server, repo: &Repository) -> Result<Vec<MergedPullForRelease>> {
    let routed = app::repository_cell(server, repo)?;
    let mut pulls = Vec::new();
    let mut before = None;
    loop {
        let (page, next) = app::query_output(
            routed
                .client
                .query::<ListMergedPullsForRelease>(&routed.target, None, before)
                .await,
        )?;
        pulls.extend(page);
        if pulls.len() > MAX_MERGED_PULLS {
            return Err(Error::Invalid(
                "This repository has too many merged pull requests to generate notes automatically",
            ));
        }
        let Some(next) = next else { return Ok(pulls) };
        before = Some(next);
    }
}

fn markdown(
    repo: &Repository,
    target: ObjectId,
    previous: Option<ObjectId>,
    pulls: impl Iterator<Item = MergedPullForRelease>,
) -> Result<String> {
    let mut body = String::from("## What's changed\n\n");
    let mut contributors = Vec::new();
    let mut authors = HashSet::new();
    for pull in pulls {
        let title = markdown_text(&pull.title);
        let author = markdown_text(&pull.author.name);
        body.push_str(&format!(
            "- {title} by {author} in [#{}](/api/discussion-references/{}/pull/{})\n",
            pull.number, repo.id, pull.number
        ));
        if authors.insert((pull.author.issuer, pull.author.subject)) {
            contributors.push(author);
        }
    }
    if contributors.is_empty() {
        body.push_str("No merged pull requests in this range.\n");
    } else {
        body.push_str("\n## Contributors\n\n");
        for author in contributors {
            body.push_str(&format!("- {author}\n"));
        }
    }
    if let Some(previous) = previous {
        body.push_str(&format!(
            "\n[Full changelog](/{}/{}/commits/{target}?since={previous}&repository_id={})\n",
            repo.config.owner, repo.config.name, repo.id
        ));
    }
    if app::body(&body, false).is_err() {
        return Err(Error::Invalid(
            "The generated notes exceed 64 KiB. Choose a more recent previous tag.",
        ));
    }
    Ok(body)
}

fn markdown_text(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        // Encode text inside an existing list item so PR metadata cannot
        // introduce headings, destinations, tables, or HTML into the draft.
        if character.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    escaped
}

#[cfg(test)]
mod tests;
