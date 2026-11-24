use super::*;
use crate::{
    api::issues::transferred::{self, PublishedIssue},
    cells::{organization::issue_types::Definition, repository::collaboration::SubjectKind},
};
use futures_util::{StreamExt, TryStreamExt, stream};

type Key = (Uuid, u8, u64);

fn key(repository: Uuid, subject: Subject) -> Key {
    (repository, subject.kind as u8, subject.number)
}

pub(super) struct Row {
    pub repository: Arc<Repository>,
    pub details: source::WorkDetails,
    pub assignees: Vec<Value>,
    pub issue_type: Option<Definition>,
    pub type_version: Option<u64>,
}

pub(super) struct Sources {
    work: BTreeMap<Key, Row>,
    moves: BTreeMap<Key, PublishedIssue>,
    pub types: Option<Vec<Definition>>,
}

impl Sources {
    pub(super) fn find(&self, repository: Uuid, subject: Subject) -> Option<&Row> {
        let origin = key(repository, subject);
        let current = self.moves.get(&origin).map_or(origin, |moved| {
            (moved.repository.id, subject.kind as u8, moved.number)
        });
        self.work.get(&current)
    }

    pub(super) async fn recheck_moves(&mut self, server: &Server) -> Result<()> {
        self.moves = stream::iter(std::mem::take(&mut self.moves))
            .map(|(key, moved)| async move {
                let visible = moved.visible(server).await?;
                Ok::<_, Error>(visible.then_some((key, moved)))
            })
            .buffer_unordered(8)
            .try_filter_map(|value| async { Ok(value) })
            .try_collect()
            .await?;
        Ok(())
    }
}

pub(super) struct Scope<'a> {
    pub server: &'a Server,
    pub principal: &'a Principal,
    pub organization: Uuid,
    pub show_issue_type: bool,
    pub type_scope: Option<&'a type_filter::FilteredItems>,
}

impl Scope<'_> {
    pub(super) async fn load(&self, groups: BTreeMap<Uuid, Vec<Subject>>) -> Result<Sources> {
        let mut sources = Sources {
            work: BTreeMap::new(),
            moves: BTreeMap::new(),
            types: self.type_scope.and_then(|filter| {
                filter
                    .types
                    .as_ref()
                    .map(|schema| schema.definitions.clone())
            }),
        };
        let mut missing = Vec::new();
        for (id, subjects) in groups {
            let Some(repo) = accessible(self.server, self.principal, id).await? else {
                continue;
            };
            self.enrich(&mut sources, &repo, &subjects).await?;
            missing.extend(subjects.into_iter().filter_map(|subject| {
                (subject.kind == SubjectKind::Issue
                    && !sources.work.contains_key(&key(repo.id, subject)))
                .then(|| (Arc::clone(&repo), subject))
            }));
        }

        // A Project placement keeps its original address. Only missing issues
        // need to follow a move before their current sources can be enriched.
        sources.moves = stream::iter(missing)
            .map(|(repo, subject)| async move {
                Ok::<_, Error>(
                    transferred::published(self.server, self.principal, &repo, subject.number)
                        .await?
                        .map(|moved| (key(repo.id, subject), moved)),
                )
            })
            .buffer_unordered(8)
            .try_filter_map(|value| async { Ok(value) })
            .try_collect()
            .await?;

        let mut destinations = BTreeMap::new();
        for moved in sources.moves.values() {
            let subject = Subject {
                kind: SubjectKind::Issue,
                number: moved.number,
            };
            if !sources
                .work
                .contains_key(&key(moved.repository.id, subject))
            {
                let (_, subjects) = destinations
                    .entry(moved.repository.id)
                    .or_insert_with(|| (Arc::clone(&moved.repository), Vec::new()));
                if !subjects.contains(&subject) {
                    subjects.push(subject);
                }
            }
        }
        for (_, (repo, subjects)) in destinations {
            self.enrich(&mut sources, &repo, &subjects).await?;
        }
        Ok(sources)
    }

    async fn enrich(
        &self,
        sources: &mut Sources,
        repo: &Arc<Repository>,
        subjects: &[Subject],
    ) -> Result<()> {
        // Keep the row's type in the same repository snapshot as its group count.
        let include_issue_type = self.show_issue_type
            && repo.organization == Some(self.organization)
            && self
                .type_scope
                .is_none_or(|filter| filter.contains_source(repo));
        let found = summaries(self.server, repo, subjects.to_vec(), include_issue_type).await?;
        let selection = match crate::api::assignees::Selection::load(
            self.server,
            self.principal,
            repo,
            found
                .iter()
                .flatten()
                .flat_map(|details| details.assignees.iter()),
        )
        .await
        {
            Ok(selection) => selection,
            Err(Error::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        let needs_types =
            sources.types.is_none() && found.iter().flatten().any(|row| row.issue_type.is_some());
        if needs_types {
            sources.types = Some(
                issue_types::active(self.server, Some(self.organization))
                    .await?
                    .definitions,
            );
        }
        // Reject a grant or ownership change after the source reads and lookups.
        let current = match app::membership(self.server, self.principal, repo).await {
            Ok(current) => current,
            Err(Error::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        for (subject, details) in subjects.iter().copied().zip(found) {
            let Some(details) = details else { continue };
            let (issue_type, type_version) = issue_types::project_field(
                repo,
                details.issue_type,
                sources.types.as_deref().unwrap_or_default(),
                current.principal.can_triage(repo.id),
            );
            let assignees = selection.view(&details.assignees);
            sources.work.insert(
                key(repo.id, subject),
                Row {
                    repository: Arc::clone(repo),
                    details,
                    assignees,
                    issue_type: issue_type.cloned(),
                    type_version,
                },
            );
        }
        Ok(())
    }
}

async fn summaries(
    server: &Server,
    repo: &Repository,
    subjects: Vec<Subject>,
    include_issue_type: bool,
) -> Result<Vec<Option<source::WorkDetails>>> {
    let expected = subjects.len();
    let cell = app::repository_cell(server, repo)?;
    let found = app::query_output(
        cell.client
            .query::<source::WorkSummaries>(
                &cell.target,
                None,
                source::WorkQuery {
                    subjects,
                    default_ref: repo.default_ref().await?,
                    include_issue_type,
                },
            )
            .await,
    )?;
    if found.len() != expected {
        return Err(Error::CellContract(
            "Project work lookup returned an invalid result count",
        ));
    }
    Ok(found)
}

pub(super) async fn can_use_source(
    server: &Server,
    principal: &Principal,
    repository: Uuid,
    subject: Subject,
    existing: bool,
) -> Result<bool> {
    let Some(repo) = accessible(server, principal, repository).await? else {
        return Ok(false);
    };
    let found = summaries(server, &repo, vec![subject], false).await?;
    match found.into_iter().next().flatten() {
        Some(_) => Ok(accessible(server, principal, repository).await?.is_some()),
        None if existing && subject.kind == SubjectKind::Issue => {
            let Some(moved) =
                transferred::published(server, principal, &repo, subject.number).await?
            else {
                return Ok(false);
            };
            // Only an existing placement can write through its stored origin.
            // The Project Cell still binds the address to the item revision.
            moved.visible(server).await
        }
        None => Ok(false),
    }
}
