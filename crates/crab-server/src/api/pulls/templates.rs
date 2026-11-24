use std::{collections::HashSet, sync::Arc};

use axum::{
    Extension, Json,
    extract::{Path, State},
};
use crab_remote_git::{
    Error as GitError, GitPath, OperationKind, PageRequest, RepositoryOptions, Revision,
};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    api::community_defaults,
    app::{self, Error, Result},
    auth::Principal,
    repository::Repository,
    server::Server,
};

const ROOTS: [&str; 3] = [".github/", "", "docs/"];
const NAMED_LIMIT: usize = 32;

#[derive(Default, Serialize)]
pub(super) struct Catalog {
    default: Option<Template>,
    items: Vec<Template>,
    truncated: bool,
    source: Option<Source>,
}

impl Catalog {
    fn has_definitions(&self) -> bool {
        self.default.is_some() || !self.items.is_empty() || self.truncated
    }
}

#[derive(Serialize)]
struct Template {
    file: String,
    path: String,
    body: String,
}

#[derive(Serialize)]
struct Source {
    id: Uuid,
    owner: String,
    name: String,
    commit: String,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Catalog>> {
    let repo = app::repository(&server, &principal, &key)?;
    let (mut catalog, _) = load(&server, &repo).await?;
    let principal = app::membership(&server, &principal, &repo).await?.principal;

    if !catalog.has_definitions()
        && let Some(source) = community_defaults::source(&server, &repo)
        && community_defaults::readable(&server, &principal, &repo, &source).await?
    {
        let (mut inherited, commit) = load(&server, &source).await?;
        if inherited.has_definitions()
            && let Some(commit) = commit
        {
            // The shared repository may be private. Recheck both grants after
            // reading its files so a stale scope cannot disclose their contents.
            let principal = app::membership(&server, &principal, &repo).await?.principal;
            if !community_defaults::readable(&server, &principal, &repo, &source).await? {
                return Err(Error::NotFound);
            }
            inherited.source = Some(Source {
                id: source.id,
                owner: source.config.owner.clone(),
                name: source.config.name.clone(),
                commit,
            });
            catalog = inherited;
        }
    }
    Ok(Json(catalog))
}

async fn load(server: &Server, repo: &Repository) -> Result<(Catalog, Option<String>)> {
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let git = repo
        .open_current(server, RepositoryOptions::default(), &cancel)
        .await?;
    let Some(head) = git.refs().head.as_ref() else {
        return Ok((Catalog::default(), None));
    };
    let revision = Revision::Reference(head.name.clone());
    let operation = git
        .operation(OperationKind::Tree, &cancel)
        .await
        .map_err(crate::Error::from)?;
    let outcome = async {
        let snapshot = git.snapshot(&revision, &operation).await?;
        let mut catalog = Catalog::default();
        let mut named = HashSet::new();
        let page = PageRequest::new(128, None)?;
        for root in ROOTS {
            if catalog.default.is_none() {
                for file in ["pull_request_template.md", "PULL_REQUEST_TEMPLATE.md"] {
                    let path = format!("{root}{file}");
                    let entry = match snapshot
                        .entry(&GitPath::new(path.as_bytes().to_vec())?, &operation)
                        .await
                    {
                        Err(GitError::PathComponentNotTree { .. }) => None,
                        result => result?,
                    };
                    if let Some(entry) = entry
                        && let Some(body) =
                            community_defaults::template_text(&snapshot, &entry, &operation).await?
                    {
                        catalog.default = Some(Template {
                            file: file.into(),
                            path,
                            body,
                        });
                        break;
                    }
                }
            }

            let directory = format!("{root}PULL_REQUEST_TEMPLATE");
            let entries = match snapshot
                .list_directory(
                    &GitPath::new(directory.as_bytes().to_vec())?,
                    &page,
                    &operation,
                )
                .await
            {
                Err(GitError::PathNotFound | GitError::PathComponentNotTree { .. }) => continue,
                result => result?,
            };
            catalog.truncated |= entries.next.is_some();
            for entry in entries.items {
                let Some((path, file)) = std::str::from_utf8(entry.path.as_bytes())
                    .ok()
                    .and_then(|path| path.rsplit_once('/').map(|(_, file)| (path, file)))
                    .filter(|(_, file)| file.ends_with(".md") && !named.contains(*file))
                else {
                    continue;
                };
                if catalog.items.len() == NAMED_LIMIT {
                    catalog.truncated = true;
                    break;
                }
                if let Some(body) =
                    community_defaults::template_text(&snapshot, &entry, &operation).await?
                {
                    named.insert(file.to_owned());
                    catalog.items.push(Template {
                        file: file.into(),
                        path: path.into(),
                        body,
                    });
                }
            }
        }
        catalog
            .items
            .sort_by(|left, right| left.file.cmp(&right.file));
        Ok((catalog, Some(snapshot.commit_oid().to_string())))
    }
    .await;
    Ok(operation
        .finish(outcome)
        .await
        .map_err(crate::Error::from)?)
}
