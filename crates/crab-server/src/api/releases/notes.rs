use std::collections::{HashMap, HashSet};

use crab_remote_git::{
    CommitMembership, OperationContext, OperationKind, RemoteGitRepository, RemoteGitSnapshot,
    Revision, RevisionError,
};

use super::*;
use crate::cells::repository::{ListMergedPullsForRelease, MergedPullForRelease};

mod configuration;
use configuration::Rules;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    target_oid: String,
    previous_tag: Option<String>,
    configuration_file_path: Option<String>,
}

#[derive(Serialize)]
pub(super) struct GeneratedNotes {
    target_oid: String,
    previous_tag: Option<String>,
    previous_oid: Option<String>,
    configuration_file: Option<String>,
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
        parameters.configuration_file_path,
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
    configuration_file_path: Option<String>,
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
    let mut target_membership = target
        .commit_membership_reader(operation)
        .map_err(crate::Error::from)?;
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
        None => {
            published_predecessor(server, repo, git, target_oid, &mut target_membership).await?
        }
    };
    let (configuration_file, rules) = configuration::load(
        server,
        repo,
        &target,
        operation,
        configuration_file_path.as_deref(),
    )
    .await?;
    let previous_oid = previous.as_ref().map(|(_, oid)| *oid);
    let notes = if previous_oid == Some(target_oid) {
        Notes::new(&rules)
    } else {
        let base = if let Some(previous_oid) = previous_oid {
            if target_membership
                .contains(&[previous_oid])
                .await
                .map_err(crate::Error::from)?
                != [true]
            {
                return Err(Error::Invalid(
                    "The previous tag must point to the release target or one of its ancestors",
                ));
            }
            Some(
                git.snapshot(&Revision::Commit(previous_oid), operation)
                    .await
                    .map_err(crate::Error::from)?,
            )
        } else {
            None
        };
        let mut base_membership = base
            .as_ref()
            .map(|base| base.commit_membership_reader(operation))
            .transpose()
            .map_err(crate::Error::from)?;
        collect_notes(
            server,
            repo,
            &rules,
            &mut target_membership,
            base_membership.as_mut(),
        )
        .await?
    };
    Ok(GeneratedNotes {
        target_oid: target_oid.to_string(),
        previous_oid: previous_oid.map(|oid| oid.to_string()),
        previous_tag: previous.map(|(name, _)| name),
        configuration_file,
        body: notes.render(repo, target_oid, previous_oid, &rules)?,
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
    target: ObjectId,
    membership: &mut CommitMembership<'_>,
) -> Result<Option<(String, ObjectId)>> {
    let tags = git
        .refs()
        .entries
        .iter()
        .filter_map(|entry| {
            let oid = entry.peeled.unwrap_or(entry.target);
            let name = entry.name.strip_prefix("refs/tags/")?;
            (oid != target).then_some((name, entry.target, oid))
        })
        .collect::<Vec<_>>();
    let candidates = tags.iter().map(|(_, _, oid)| *oid).collect::<Vec<_>>();
    let included = membership
        .contains(&candidates)
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

async fn collect_notes(
    server: &Server,
    repo: &Repository,
    rules: &Rules,
    target: &mut CommitMembership<'_>,
    mut base: Option<&mut CommitMembership<'_>>,
) -> Result<Notes> {
    let routed = app::repository_cell(server, repo)?;
    let mut notes = Notes::new(rules);
    let mut before = None;
    loop {
        let (page, next) = app::query_output(
            routed
                .client
                .query::<ListMergedPullsForRelease>(&routed.target, None, before)
                .await,
        )?;
        let candidates = page
            .iter()
            .map(|pull| target_oid_from_merge(&pull.commit_oid))
            .collect::<Result<Vec<_>>>()?;
        let mut included = target
            .contains(&candidates)
            .await
            .map_err(crate::Error::from)?;
        if let Some(base) = base.as_deref_mut()
            && included.iter().any(|include| *include)
        {
            let excluded = base
                .contains(&candidates)
                .await
                .map_err(crate::Error::from)?;
            for (include, exclude) in included.iter_mut().zip(excluded) {
                *include &= !exclude;
            }
        }
        for (pull, include) in page.into_iter().zip(included) {
            if include && let Some(section) = rules.section(&pull) {
                notes.append(repo.id, section, pull)?;
            }
        }
        let Some(next) = next else { return Ok(notes) };
        before = Some(next);
    }
}

struct Notes {
    contributors: Vec<String>,
    authors: HashSet<(String, String)>,
    sections: Vec<String>,
    bytes: usize,
}

impl Notes {
    fn new(rules: &Rules) -> Self {
        Self {
            contributors: Vec::new(),
            authors: HashSet::new(),
            sections: vec![String::new(); rules.categories.len().max(1)],
            bytes: 0,
        }
    }

    fn append(
        &mut self,
        repository: uuid::Uuid,
        section: usize,
        pull: MergedPullForRelease,
    ) -> Result<()> {
        let title = markdown_text(&pull.title);
        let author = markdown_text(&pull.author.name);
        let line = format!(
            "- {title} by {author} in [#{}](/api/discussion-references/{}/pull/{})\n",
            pull.number, repository, pull.number
        );
        self.bytes += line.len();
        if self
            .authors
            .insert((pull.author.issuer, pull.author.subject))
        {
            self.bytes += author.len() + "- \n".len();
            self.contributors.push(author);
        }
        if self.bytes > app::MAX_TEXT_BODY_BYTES {
            return Err(notes_too_large());
        }
        self.sections[section].push_str(&line);
        Ok(())
    }

    fn render(
        self,
        repo: &Repository,
        target: ObjectId,
        previous: Option<ObjectId>,
        rules: &Rules,
    ) -> Result<String> {
        let mut body = String::from("## What's changed\n\n");
        if self.contributors.is_empty() {
            body.push_str("No merged pull requests in this range.\n");
        } else {
            for (index, section) in self
                .sections
                .into_iter()
                .enumerate()
                .filter(|(_, text)| !text.is_empty())
            {
                if let Some(category) = rules.categories.get(index) {
                    body.push_str(&format!("### {}\n\n", markdown_text(&category.title)));
                }
                body.push_str(&section);
                body.push('\n');
            }
            body.push_str("## Contributors\n\n");
            for author in self.contributors {
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
            return Err(notes_too_large());
        }
        Ok(body)
    }
}

fn notes_too_large() -> Error {
    Error::Invalid("The generated notes exceed 64 KiB. Choose a more recent previous tag.")
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
