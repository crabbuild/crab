use super::*;
use crate::{
    api::{
        community_defaults,
        members::roster::{Candidate, Roster},
    },
    cells::repository::collaboration::creation::Selection as Metadata,
    repository::Repository,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use crab_remote_git::{GitPath, OperationKind, PageRequest, RepositoryOptions, Revision};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;

mod document;
use document::Document;

const DIRECTORY: &str = ".github/ISSUE_TEMPLATE";
const LIMIT: usize = 32;
const GRANT_MS: i64 = 60 * 60 * 1000;

#[derive(Default)]
struct Catalog {
    templates: Vec<(String, Document)>,
    unavailable: Vec<String>,
    truncated: bool,
    config: Option<Config>,
}

impl Catalog {
    fn overrides_defaults(&self) -> bool {
        // A partial listing cannot prove the destination has no local templates.
        self.config.is_some() || !self.templates.is_empty() || self.truncated
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    blank_issues_enabled: Option<bool>,
    contact_links: Vec<Contact>,
}

#[derive(Deserialize, Serialize)]
struct Contact {
    name: String,
    about: String,
    url: String,
}

impl Contact {
    fn valid(&self) -> bool {
        document::line(&self.name, 128)
            && self.about.len() <= 1024
            && self.url.len() <= 2048
            && url::Url::parse(&self.url).is_ok_and(|url| {
                matches!(url.scheme(), "https" | "http")
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
            })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Grant {
    expires_at_ms: i64,
    proof: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<uuid::Uuid>,
}

fn signature(
    server: &Server,
    repo: &Repository,
    author: &crate::auth::Identity,
    metadata: &Metadata,
    expires_at_ms: i64,
    source: Option<uuid::Uuid>,
) -> Result<Hmac<Sha256>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(&server.cursor_key)
        .map_err(|_| Error::CellContract("Invalid issue template key"))?;
    mac.update(b"crab.issue-template.defaults.v1\0");
    mac.update(&serde_json::to_vec(&(
        repo.id,
        &author.issuer,
        &author.subject,
        metadata,
        expires_at_ms,
    ))?);
    if let Some(source) = source {
        mac.update(source.as_bytes());
    }
    Ok(mac)
}

impl Grant {
    pub(super) async fn allows(
        &self,
        server: &Server,
        repo: &Repository,
        principal: &Principal,
        metadata: &Metadata,
    ) -> Result<bool> {
        if metadata.milestone.is_some()
            || self.expires_at_ms < crate::cells::unix_now_ms().map_err(Error::Cell)?
            || self.proof.len() != 43
        {
            return Ok(false);
        }
        let Ok(proof) = URL_SAFE_NO_PAD.decode(&self.proof) else {
            return Ok(false);
        };
        let author = actor(principal)?;
        if signature(
            server,
            repo,
            &author,
            metadata,
            self.expires_at_ms,
            self.source,
        )?
        .verify_slice(&proof)
        .is_err()
        {
            return Ok(false);
        }
        let Some(source_id) = self.source else {
            return Ok(true);
        };
        let Some(source) =
            community_defaults::source(server, repo).filter(|source| source.id == source_id)
        else {
            return Ok(false);
        };
        community_defaults::readable(server, principal, repo, &source).await
    }
}

async fn resolved_catalog(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
) -> Result<(Catalog, Option<Arc<Repository>>)> {
    let local = load(server, repo).await?;
    if local.overrides_defaults() {
        return Ok((local, None));
    }
    let Some(source) = community_defaults::source(server, repo) else {
        return Ok((local, None));
    };
    if !community_defaults::readable(server, principal, repo, &source).await? {
        return Ok((local, None));
    }
    let mut inherited = load(server, &source).await?;
    if !inherited.overrides_defaults() {
        return Ok((local, None));
    }
    // Preserve visibility of malformed local definitions when defaults win.
    inherited.unavailable.extend(
        local
            .unavailable
            .into_iter()
            .map(|file| format!("{}: {file}", repo.config.name)),
    );
    Ok((inherited, Some(source)))
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Value>> {
    let repo = repository(&server, &principal, &key)?;
    let (catalog, source) = resolved_catalog(&server, &principal, &repo).await?;
    let (labels, roster) = tokio::try_join!(
        async {
            if catalog
                .templates
                .iter()
                .any(|(_, template)| !template.header.labels().is_empty())
            {
                labels::catalog(&server, &repo).await
            } else {
                Ok(vec![])
            }
        },
        async {
            if catalog
                .templates
                .iter()
                .any(|(_, template)| !template.header.assignees().is_empty())
            {
                Ok(Roster::load(&server, &principal, &repo).await?.items)
            } else {
                Ok(vec![])
            }
        },
    )?;
    let principal = app::membership(&server, &principal, &repo).await?.principal;
    if let Some(source) = &source
        && !community_defaults::readable(&server, &principal, &repo, source).await?
    {
        return Err(Error::NotFound);
    }
    let author = actor(&principal)?;
    let source_id = source.as_ref().map(|source| source.id);
    let expires_at_ms = crate::cells::unix_now_ms()
        .map_err(Error::Cell)?
        .checked_add(GRANT_MS)
        .ok_or(Error::CellContract(
            "Issue template expiry exceeds timestamp range",
        ))?;
    let items = catalog.templates.into_iter().map(|(file, template)| {
        let defaults = template.header.labels();
        let mut label_ids: Vec<_> = labels.iter().filter(|label| defaults.iter().any(|name| name.to_lowercase() == label.name.to_lowercase())).map(|label| label.number).collect();
        label_ids.sort_unstable();
        let handles = template.header.assignees();
        let assignees: Vec<&Candidate> = roster.iter().filter(|person| person.profile.as_ref().is_some_and(|profile| {
            handles.iter().any(|handle| handle.trim_start_matches('@').eq_ignore_ascii_case(&profile.handle))
        })).collect();
        let mut assignee_ids: Vec<_> = assignees.iter().map(|person| person.id).collect();
        assignee_ids.sort_unstable();
        let metadata = Metadata { label_ids, assignee_ids, milestone: None };
        let grant = if metadata.is_empty() { None } else {
            let proof = URL_SAFE_NO_PAD.encode(signature(&server, &repo, &author, &metadata, expires_at_ms, source_id)?.finalize().into_bytes());
            Some(Grant { expires_at_ms, proof, source: source_id })
        };
        Ok(json!({
            "file":file, "name":template.header.name(), "description":template.header.about(),
            "title":template.header.title, "content":template.content,
            "labels":labels::selection_view(&metadata.label_ids, &labels), "assignees":assignees,
            "grant":grant,
        }))
    }).collect::<Result<Vec<_>>>()?;
    let config = catalog.config.unwrap_or_default();
    let blank_public = config.blank_issues_enabled.unwrap_or(true);
    let contacts: Vec<_> = config
        .contact_links
        .into_iter()
        .filter(Contact::valid)
        .take(20)
        .collect();
    Ok(Json(
        json!({"items":items,"contact_links":contacts,"unavailable":catalog.unavailable,"truncated":catalog.truncated,
        "blank_allowed":blank_public || principal.can_write(repo.id), "blank_maintainers_only":!blank_public,
        "source":source.map(|source| json!({"owner":source.config.owner,"name":source.config.name}))}),
    ))
}

async fn load(server: &Server, repo: &Repository) -> Result<Catalog> {
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let git = repo
        .open_current(server, RepositoryOptions::default(), &cancel)
        .await?;
    let Some(head) = git.refs().head.as_ref() else {
        return Ok(Catalog::default());
    };
    let revision = Revision::Reference(head.name.clone());
    let operation = git
        .operation(OperationKind::Tree, &cancel)
        .await
        .map_err(crate::Error::from)?;
    let outcome = async {
        use crab_remote_git::Error as GitError;
        let snapshot = git.snapshot(&revision, &operation).await?;
        let path = GitPath::new(DIRECTORY.as_bytes().to_vec())?;
        let page = PageRequest::new(128, None)?;
        let entries = match snapshot.list_directory(&path, &page, &operation).await {
            Err(GitError::PathNotFound | GitError::PathComponentNotTree { .. }) => {
                return Ok(Catalog::default());
            }
            result => result?,
        };
        let mut found = Catalog {
            truncated: entries.next.is_some(),
            ..Default::default()
        };
        let config_path = GitPath::new(format!("{DIRECTORY}/config.yml").into_bytes())?;
        if let Some(entry) = snapshot.entry(&config_path, &operation).await? {
            if let Some(config) = community_defaults::template_text(&snapshot, &entry, &operation)
                .await?
                .and_then(|source| serde_yaml::from_str(&source).ok())
            {
                found.config = Some(config);
            } else {
                found.unavailable.push("config.yml".into());
            }
        }
        for entry in entries.items {
            let Ok(path) = std::str::from_utf8(entry.path.as_bytes()) else {
                continue;
            };
            let Some(name) = path
                .strip_prefix(DIRECTORY)
                .and_then(|path| path.strip_prefix('/'))
            else {
                continue;
            };
            if name == "config.yml"
                || (!name.ends_with(".md") && !name.ends_with(".yml") && !name.ends_with(".yaml"))
            {
                continue;
            }
            if found.templates.len() + found.unavailable.len() >= LIMIT {
                found.truncated = true;
                continue;
            }
            if let Some(parsed) = community_defaults::template_text(&snapshot, &entry, &operation)
                .await?
                .and_then(|source| Document::parse(name, &source))
            {
                found.templates.push((name.into(), parsed));
            } else {
                found.unavailable.push(name.into());
            }
        }
        found.templates.sort_by(|(a, _), (b, _)| {
            a.ends_with(".md")
                .cmp(&b.ends_with(".md"))
                .then_with(|| a.cmp(b))
        });
        Ok(found)
    }
    .await;
    Ok(operation
        .finish(outcome)
        .await
        .map_err(crate::Error::from)?)
}
