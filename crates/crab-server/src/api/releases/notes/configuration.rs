use crab_remote_git::{ContentClassification, EntryKind, GitPath};

use super::*;
use crate::cells::{
    Address,
    directory::{IdentityKey, ResolveHandles, validate_handle},
    repository::ListLabels,
};

const FILE_BYTES: u64 = 32 * 1024;
const MAX_CATEGORIES: usize = 32;
const MAX_SELECTORS: usize = 128;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    changelog: Changelog,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
struct Changelog {
    exclude: Filter,
    categories: Vec<Category>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Filter {
    labels: Vec<String>,
    authors: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Category {
    title: String,
    labels: Vec<String>,
    #[serde(default)]
    exclude: Filter,
}

#[derive(Default)]
pub(super) struct Rules {
    exclude: Selection,
    pub(super) categories: Vec<Section>,
}

#[derive(Default)]
struct Selection {
    labels: HashSet<u64>,
    authors: Vec<IdentityKey>,
}

pub(super) struct Section {
    pub(super) title: String,
    labels: HashSet<u64>,
    all: bool,
    exclude: Selection,
}

impl Rules {
    pub(super) fn section(&self, pull: &MergedPullForRelease) -> Option<usize> {
        if self.exclude.matches(pull) {
            return None;
        }
        if self.categories.is_empty() {
            return Some(0);
        }
        self.categories.iter().position(|category| {
            !category.exclude.matches(pull)
                && (category.all || pull.label_ids.iter().any(|id| category.labels.contains(id)))
        })
    }
}

impl Selection {
    fn matches(&self, pull: &MergedPullForRelease) -> bool {
        pull.label_ids.iter().any(|id| self.labels.contains(id))
            || self.authors.iter().any(|actor| {
                pull.author.issuer == actor.issuer && pull.author.subject == actor.subject
            })
    }
}

pub(super) async fn load(
    server: &Server,
    repo: &Repository,
    target: &RemoteGitSnapshot,
    operation: &OperationContext,
    selected: Option<&str>,
) -> Result<(Option<String>, Rules)> {
    let defaults = [".github/release.yml", ".github/release.yaml"];
    let paths = selected
        .as_ref()
        .map_or(defaults.as_slice(), std::slice::from_ref);
    for file in paths {
        if file.len() > 1024
            || file.trim() != *file
            || file.contains('\\')
            || file.chars().any(char::is_control)
            || file.split('/').any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(Error::Invalid(
                "Enter a repository-relative release note configuration path",
            ));
        }
        let path = GitPath::new(file.as_bytes().to_vec()).map_err(crate::Error::from)?;
        let entry = match target.entry(&path, operation).await {
            Ok(Some(entry)) => entry,
            Ok(None) | Err(crab_remote_git::Error::PathComponentNotTree { .. })
                if selected.is_none() =>
            {
                continue;
            }
            Ok(None) | Err(crab_remote_git::Error::PathComponentNotTree { .. }) => {
                return Err(Error::Invalid(
                    "Release note configuration was not found at the selected commit",
                ));
            }
            Err(error) => return Err(crate::Error::from(error).into()),
        };
        if entry.kind != EntryKind::Blob
            || operation
                .read_object_metadata(entry.oid)
                .await
                .map_err(crate::Error::from)?
                .size
                > FILE_BYTES
        {
            return Err(Error::Invalid(
                "Release note configuration must be a Git file of at most 32 KiB",
            ));
        }
        let blob = target
            .read_blob(&path, operation)
            .await
            .map_err(crate::Error::from)?;
        if blob.metadata.classification != ContentClassification::OrdinaryGit {
            return Err(Error::Invalid(
                "Release note configuration must be stored as plain Git text",
            ));
        }
        let document: Document =
            serde_yaml::from_slice(&blob.bytes).map_err(Error::ReleaseNotesYaml)?;
        return Ok((
            Some((*file).to_owned()),
            document.changelog.resolve(server, repo).await?,
        ));
    }
    Ok((None, Rules::default()))
}

impl Changelog {
    async fn resolve(self, server: &Server, repo: &Repository) -> Result<Rules> {
        let selections =
            std::iter::once(&self.exclude).chain(self.categories.iter().map(|item| &item.exclude));
        let wanted = selections
            .clone()
            .flat_map(|selection| &selection.authors)
            .map(|name| {
                let handle = name.strip_prefix('@').unwrap_or(name).to_ascii_lowercase();
                validate_handle(&handle).map_err(Error::Invalid)?;
                Ok(handle)
            })
            .collect::<Result<HashSet<_>>>()?;
        let label_count = selections
            .clone()
            .map(|selection| selection.labels.len())
            .sum::<usize>()
            + self
                .categories
                .iter()
                .map(|category| category.labels.len())
                .sum::<usize>();
        if self.categories.len() > MAX_CATEGORIES
            || selections
                .clone()
                .map(|selection| selection.authors.len())
                .sum::<usize>()
                > MAX_SELECTORS
            || label_count > MAX_SELECTORS
        {
            return Err(Error::Invalid(
                "Use at most 32 release note categories, 128 label entries and 128 author entries",
            ));
        }
        for category in &self.categories {
            app::title(&category.title)?;
            if category.labels.is_empty() {
                return Err(Error::Invalid(
                    "Each release note category needs a label or the '*' catch-all",
                ));
            }
        }
        let labels_iter = selections
            .flat_map(|selection| &selection.labels)
            .chain(self.categories.iter().flat_map(|category| &category.labels));
        for label in labels_iter {
            if label.is_empty()
                || label.trim() != label
                || label.chars().count() > 50
                || label.chars().any(char::is_control)
            {
                return Err(Error::Invalid(
                    "Release note labels must be valid repository label names",
                ));
            }
        }

        let labels: HashMap<_, _> = if label_count == 0 {
            HashMap::new()
        } else {
            let routed = app::repository_cell(server, repo)?;
            app::query_output(
                routed
                    .client
                    .query::<ListLabels>(&routed.target, None, ())
                    .await,
            )?
            .labels
            .into_iter()
            .map(|label| (label.name.to_lowercase(), label.number))
            .collect()
        };
        let handles = resolve_handles(server, wanted).await?;
        let label_ids = |names: &[String]| {
            names
                .iter()
                .filter_map(|name| labels.get(&name.to_lowercase()).copied())
                .collect()
        };
        let selection = |filter: Filter| Selection {
            labels: label_ids(&filter.labels),
            authors: filter
                .authors
                .iter()
                .filter_map(|name| {
                    let handle = name.strip_prefix('@').unwrap_or(name).to_ascii_lowercase();
                    handles.get(&handle).cloned()
                })
                .collect(),
        };
        let categories = self
            .categories
            .into_iter()
            .map(|category| Section {
                title: category.title.trim().to_owned(),
                labels: label_ids(&category.labels),
                all: category.labels.iter().any(|label| label == "*"),
                exclude: selection(category.exclude),
            })
            .collect();
        Ok(Rules {
            exclude: selection(self.exclude),
            categories,
        })
    }
}

async fn resolve_handles(
    server: &Server,
    wanted: HashSet<String>,
) -> Result<HashMap<String, IdentityKey>> {
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let names = wanted.into_iter().collect::<Vec<_>>();
    let mut handles = HashMap::new();
    for batch in names.chunks(64) {
        let identities = app::query_output(
            cells
                .query::<ResolveHandles>(Address::Directory, None, batch.to_vec())
                .await,
        )?;
        if identities.len() != batch.len() {
            return Err(Error::CellContract(
                "Release note author lookup returned an invalid count",
            ));
        }
        handles.extend(
            batch
                .iter()
                .cloned()
                .zip(identities)
                .filter_map(|(name, identity)| identity.map(|identity| (name, identity))),
        );
    }
    Ok(handles)
}
