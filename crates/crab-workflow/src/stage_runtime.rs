//! Core stage definitions: [`Stage`], [`StageName`], [`Cmd`], [`Dep`],
//! [`Out`], [`OutKind`], [`EnvSpec`], [`RetryPolicy`].
//!
//! These types are the in-memory representation of a single workflow
//! stage. They are consumed by the hasher, executor, and journal, and
//! round-tripped through the lockfile via `serde`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Dep;
use crate::{ExternalHashIndex, ExternalHashRecord};
use crate::{Result, WorkflowError as CrabError};
use futures_util::StreamExt;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;

/// Resolves and content-hashes URL dependencies for stage execution.
pub trait DepUrlHashExt {
    /// Hashes a URL dependency without named remote aliases.
    #[cfg(any(test, feature = "testing"))]
    fn url_hash(&self) -> Result<Option<(String, [u8; 32])>> {
        self.url_hash_with_remote_aliases(&BTreeMap::new())
    }

    /// Hashes a URL dependency after expanding any configured remote alias.
    fn url_hash_with_remote_aliases(
        &self,
        remote_aliases: &BTreeMap<String, String>,
    ) -> Result<Option<(String, [u8; 32])>>;

    /// Hash a URL dependency with an optional per-worktree validator index.
    fn url_hash_with_remote_aliases_and_index(
        &self,
        remote_aliases: &BTreeMap<String, String>,
        index_path: Option<&Path>,
    ) -> Result<Option<(String, [u8; 32])>> {
        let _ = index_path;
        self.url_hash_with_remote_aliases(remote_aliases)
    }
}

impl DepUrlHashExt for Dep {
    fn url_hash_with_remote_aliases(
        &self,
        remote_aliases: &BTreeMap<String, String>,
    ) -> Result<Option<(String, [u8; 32])>> {
        self.url_hash_with_remote_aliases_and_index(remote_aliases, None)
    }

    fn url_hash_with_remote_aliases_and_index(
        &self,
        remote_aliases: &BTreeMap<String, String>,
        index_path: Option<&Path>,
    ) -> Result<Option<(String, [u8; 32])>> {
        let Dep::Url { url, digest } = self else {
            return Ok(None);
        };
        let digest = match digest {
            Some(digest) => parse_pinned_url_digest(url, digest)?,
            None => {
                let expanded = expand_remote_alias_url(url, remote_aliases, "url dep")?;
                hash_live_url_dep(expanded.as_ref(), index_path)?
            }
        };
        Ok(Some((url.clone(), digest)))
    }
}

fn expand_remote_alias_url<'a>(
    url: &'a str,
    remote_aliases: &'a BTreeMap<String, String>,
    subject: &str,
) -> Result<Cow<'a, str>> {
    let Some((scheme, _)) = url.split_once("://") else {
        return Ok(Cow::Borrowed(url));
    };
    if !scheme.eq_ignore_ascii_case("remote") {
        return Ok(Cow::Borrowed(url));
    }

    let parsed = url::Url::parse(url).map_err(|source| CrabError::Configuration {
        key: subject.to_owned(),
        origin: format!("remote alias URL is invalid: {source}"),
    })?;
    let Some(name) = parsed.host_str().filter(|name| !name.is_empty()) else {
        return Err(CrabError::Configuration {
            key: subject.to_owned(),
            origin: "remote alias URL must be remote://<name>/<path>".to_owned(),
        });
    };
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(CrabError::Configuration {
            key: subject.to_owned(),
            origin: "remote alias URL may only contain a remote name and path".to_owned(),
        });
    }

    let Some(base) = remote_aliases.get(name).map(String::as_str) else {
        return Err(CrabError::Configuration {
            key: format!("workflow.remotes.{name}"),
            origin: format!(
                "remote alias {} has no matching [workflow.remotes.{name}]",
                redacted_url_subject(url)
            ),
        });
    };
    let rel = parsed.path().trim_start_matches('/');
    let expanded = join_remote_alias_base(name, base, rel)?;
    Ok(Cow::Owned(expanded))
}

/// Expands a DVC-style `remote://name/path` output alias.
pub fn expand_external_url_out_alias(
    url: &str,
    remote_aliases: &BTreeMap<String, String>,
) -> Result<String> {
    Ok(expand_remote_alias_url(url, remote_aliases, "external output URL")?.into_owned())
}

/// Validate that a URL dependency or external output has a live runtime
/// provider before a workflow stage is scheduled. Pinned URLs still need a
/// provider for external outputs and are rejected for unsupported schemes so
/// configuration cannot fail after a child process has already started.
pub fn validate_url_provider(
    url: &str,
    remote_aliases: &BTreeMap<String, String>,
    subject: &str,
) -> Result<()> {
    let expanded = expand_remote_alias_url(url, remote_aliases, subject)?;
    let parsed = url::Url::parse(expanded.as_ref()).map_err(|source| CrabError::Configuration {
        key: "workflow.external_provider_invalid".to_owned(),
        origin: format!("{subject} URL is invalid: {source}"),
    })?;
    if url_has_embedded_credentials(&parsed) {
        return Err(CrabError::Configuration {
            key: "workflow.external_provider_credentials".to_owned(),
            origin: format!(
                "{subject} embeds credentials; configure the provider outside crab.yaml"
            ),
        });
    }
    let scheme = parsed.scheme().to_ascii_lowercase();
    if !matches!(
        scheme.as_str(),
        "http" | "https" | "file" | "s3" | "s3a" | "gs" | "az" | "azure" | "abfs" | "abfss" | "adl"
    ) {
        return Err(CrabError::Configuration {
            key: "workflow.external_provider_unsupported".to_owned(),
            origin: format!(
                "{subject} uses unsupported provider scheme '{scheme}'; supported schemes are http, https, file, s3, gs, and Azure object stores"
            ),
        });
    }
    Ok(())
}

fn url_has_embedded_credentials(url: &url::Url) -> bool {
    !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query_pairs().any(|(key, _)| {
            let key = key.to_ascii_lowercase();
            [
                "token",
                "secret",
                "password",
                "key",
                "signature",
                "credential",
                "access",
                "auth",
            ]
            .iter()
            .any(|marker| key.contains(marker))
        })
}

fn join_remote_alias_base(name: &str, base: &str, rel: &str) -> Result<String> {
    let trimmed = base.trim();
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("workflow.remotes.{name}.url"),
            origin: "workflow remote URL must not be empty".to_owned(),
        });
    }

    // Check filesystem paths before URL parsing. Windows drive paths such
    // as `C:\\data` are accepted by `url::Url::parse` as a custom-scheme
    // URL, but remote aliases must treat them as local files.
    let base_path = Path::new(trimmed);
    if base_path.has_root() {
        let joined = if rel.is_empty() {
            base_path.to_path_buf()
        } else {
            base_path.join(rel)
        };
        return url::Url::from_file_path(&joined)
            .map(|url| url.to_string())
            .map_err(|()| CrabError::Configuration {
                key: format!("workflow.remotes.{name}.url"),
                origin: "workflow remote alias local path cannot be represented as file://"
                    .to_owned(),
            });
    }

    if let Ok(parsed) = url::Url::parse(trimmed) {
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(CrabError::Configuration {
                key: format!("workflow.remotes.{name}.url"),
                origin: "workflow remote alias base URL must not contain query or fragment"
                    .to_owned(),
            });
        }
        let mut base_url = parsed.to_string();
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        let base_url = url::Url::parse(&base_url).map_err(|source| CrabError::Configuration {
            key: format!("workflow.remotes.{name}.url"),
            origin: format!("workflow remote alias base URL is invalid: {source}"),
        })?;
        return base_url
            .join(rel)
            .map(|joined| joined.to_string())
            .map_err(|source| CrabError::Configuration {
                key: format!("workflow.remotes.{name}.url"),
                origin: format!("workflow remote alias path is invalid: {source}"),
            });
    }

    Err(CrabError::Configuration {
        key: format!("workflow.remotes.{name}.url"),
        origin: "workflow remote alias URL must be an absolute URL or absolute local path"
            .to_owned(),
    })
}

fn parse_pinned_url_digest(url: &str, digest: &str) -> Result<[u8; 32]> {
    let subject = redacted_url_subject(url);
    let hex = digest
        .strip_prefix("b3:")
        .ok_or_else(|| CrabError::Configuration {
            key: format!("url dep '{subject}' digest"),
            origin: "pinned URL deps currently require a b3:<64-hex> digest".to_owned(),
        })?;
    if hex.len() != 64 {
        return Err(CrabError::Configuration {
            key: format!("url dep '{subject}' digest"),
            origin: format!(
                "b3 digest must contain 64 hex characters, got {}",
                hex.len()
            ),
        });
    }

    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        let pair = &hex[index * 2..index * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).map_err(|_| CrabError::Configuration {
            key: format!("url dep '{subject}' digest"),
            origin: "b3 digest contains non-hex characters".to_owned(),
        })?;
    }
    Ok(out)
}

fn hash_live_url_dep(url: &str, index_path: Option<&Path>) -> Result<[u8; 32]> {
    let subject = redacted_url_subject(url);
    let parsed = url::Url::parse(url).map_err(|source| CrabError::Configuration {
        key: format!("url dep '{subject}'"),
        origin: format!("URL dependency must be an absolute URL: {source}"),
    })?;
    match parsed.scheme() {
        "http" | "https" => hash_http_url_dep(url, index_path),
        "file" => hash_file_url_dep(url, &parsed),
        "s3" | "s3a" | "gs" | "az" | "azure" | "abfs" | "abfss" | "adl" => {
            hash_object_store_url_dep(url, index_path)
        }
        _ => Err(CrabError::StageRemoteExecutionUnsupported),
    }
}

fn redacted_url_subject(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return "<redacted-url>".to_owned();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

fn hash_http_url_dep(url: &str, index_path: Option<&Path>) -> Result<[u8; 32]> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(format!("crab/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(url_dep_network_error)?;

    let index_identity = index_identity(url);
    let validator = index_path
        .and(index_identity.as_ref())
        .and_then(|(locator, scope)| {
            let head = client.head(url).send().ok()?;
            if !head.status().is_success() {
                return None;
            }
            let size = head.content_length()?;
            let validator = strong_validator(&head);
            Some((locator.clone(), scope.clone(), size, validator))
        });
    if let (Some(index_path), Some((locator, scope, size, Some(validator)))) =
        (index_path, validator.as_ref().cloned())
    {
        let index = match ExternalHashIndex::load(index_path) {
            Ok(index) => index,
            Err(error) => {
                tracing::warn!(path = %index_path.display(), error = %error, "ignoring invalid external hash index");
                ExternalHashIndex::default()
            }
        };
        if let Some(value) = index.reusable("http", &locator, &scope, size, Some(&validator))
            && let Some(hash) = parse_cached_hash(value)
        {
            return Ok(hash);
        }
    }

    let mut response = client.get(url).send().map_err(url_dep_network_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(CrabError::Storage(object_store::Error::Generic {
            store: "workflow URL dep",
            source: Box::new(std::io::Error::other(format!(
                "GET {} failed with HTTP {status}",
                redacted_url_subject(url)
            ))),
        }));
    }

    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut response, &mut hasher).map_err(CrabError::Io)?;
    let hash = *hasher.finalize().as_bytes();
    if let (Some(index_path), Some((locator, scope))) = (index_path, index_identity)
        && let Some(validator) = strong_validator(&response)
        && let Some(size) = response.content_length()
    {
        let mut index = match ExternalHashIndex::load(index_path) {
            Ok(index) => index,
            Err(error) => {
                tracing::warn!(path = %index_path.display(), error = %error, "replacing invalid external hash index");
                ExternalHashIndex::default()
            }
        };
        if index
            .insert(ExternalHashRecord {
                provider: "http".to_owned(),
                locator,
                credential_scope: scope,
                size,
                validator: Some(validator),
                last_modified: response
                    .headers()
                    .get(reqwest::header::LAST_MODIFIED)
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned),
                crab_hash: format!("b3:{}", hex_bytes(&hash)),
                observed_at_unix_ms: now_unix_ms(),
            })
            .is_ok()
            && let Err(error) = index.save_atomic(index_path)
        {
            tracing::warn!(path = %index_path.display(), error = %error, "could not persist external hash index");
        }
    }
    Ok(hash)
}

fn index_identity(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    Some((parsed.to_string(), "anonymous".to_owned()))
}

fn strong_validator(response: &reqwest::blocking::Response) -> Option<String> {
    let value = response
        .headers()
        .get(reqwest::header::ETAG)?
        .to_str()
        .ok()?;
    (!value.is_empty() && !value.trim_start().starts_with("W/")).then(|| value.to_owned())
}

fn parse_cached_hash(value: &str) -> Option<[u8; 32]> {
    let value = value.strip_prefix("b3:")?;
    if value.len() != 64 {
        return None;
    }
    let mut hash = [0_u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

fn hex_bytes(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn hash_file_url_dep(url: &str, parsed: &url::Url) -> Result<[u8; 32]> {
    let subject = redacted_url_subject(url);
    let path = parsed
        .to_file_path()
        .map_err(|()| CrabError::Configuration {
            key: format!("url dep '{subject}'"),
            origin: "file:// dependency must resolve to a local filesystem path".to_owned(),
        })?;
    let meta = std::fs::symlink_metadata(&path).map_err(CrabError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(CrabError::Configuration {
            key: format!("url dep '{subject}'"),
            origin: "file:// dependency must not follow a symlink".to_owned(),
        });
    }
    if meta.is_dir() {
        return Ok(crate::hasher::hash_directory(&path, true)?.hash);
    }
    if !meta.is_file() {
        return Err(CrabError::Configuration {
            key: format!("url dep '{subject}'"),
            origin: "file:// dependency must point to a regular file or directory".to_owned(),
        });
    }

    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(&path).map_err(CrabError::Io)?;
    std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
    Ok(*hasher.finalize().as_bytes())
}

fn url_dep_network_error(source: reqwest::Error) -> CrabError {
    CrabError::NetworkTransient(object_store::Error::Generic {
        store: "workflow URL dep",
        source: Box::new(source),
    })
}

fn hash_object_store_url_dep(url: &str, index_path: Option<&Path>) -> Result<[u8; 32]> {
    block_on_url_dep_hash(hash_object_store_url_dep_async(
        url.to_owned(),
        index_path.map(Path::to_path_buf),
    ))
}

fn block_on_url_dep_hash<F>(future: F) -> Result<[u8; 32]>
where
    F: Future<Output = Result<[u8; 32]>> + Send + 'static,
{
    let run = move || -> Result<[u8; 32]> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(CrabError::Io)?;
        runtime.block_on(future)
    };

    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(run).join().map_err(|_| {
            CrabError::Internal("workflow URL dep hash worker panicked".to_owned())
        })?;
    }

    run()
}

async fn hash_object_store_url_dep_async(
    url: String,
    index_path: Option<PathBuf>,
) -> Result<[u8; 32]> {
    let subject = redacted_url_subject(&url);
    let parsed = url::Url::parse(&url).map_err(|source| CrabError::Configuration {
        key: format!("url dep '{subject}'"),
        origin: format!("URL dependency must be an absolute URL: {source}"),
    })?;
    let options: Vec<(String, String)> = std::env::vars()
        .map(|(key, value)| (key.to_ascii_lowercase(), value))
        .collect();
    let (store, location) = object_store::parse_url_opts(
        &parsed,
        options
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .map_err(CrabError::Storage)?;
    let provider = parsed.scheme().to_owned();
    let locator = redacted_object_store_locator(&parsed);
    let scope = object_store_scope(&parsed);
    hash_object_store_location(
        store.as_ref(),
        &location,
        index_path.as_deref(),
        &provider,
        &locator,
        &scope,
    )
    .await
}

async fn hash_object_store_location(
    store: &dyn object_store::ObjectStore,
    location: &ObjectPath,
    index_path: Option<&Path>,
    provider: &str,
    locator: &str,
    credential_scope: &str,
) -> Result<[u8; 32]> {
    let object_meta = match store.head(location).await {
        Ok(meta) => Some(meta),
        Err(err) if is_object_store_not_found(&err) => None,
        Err(err) => return Err(CrabError::Storage(err)),
    };
    if let Some(meta) = object_meta {
        if let Some(validator) = object_store_validator(&meta)
            && let Some(index_path) = index_path
            && let Some(value) = load_external_hash(index_path).reusable(
                provider,
                locator,
                credential_scope,
                meta.size,
                Some(&validator),
            )
            && let Some(hash) = parse_cached_hash(value)
        {
            return Ok(hash);
        }
        let result = store.get(location).await.map_err(CrabError::Storage)?;
        let hash = hash_object_store_result(result).await?;
        if let Some(validator) = object_store_validator(&meta) {
            record_external_hash(
                index_path,
                provider,
                locator,
                credential_scope,
                meta.size,
                validator,
                Some(meta.last_modified.to_rfc3339()),
                hash,
            );
        }
        return Ok(hash);
    }

    let mut stream = store.list(Some(location));
    let root_prefix = location.as_ref().trim_end_matches('/');
    let root_child_prefix = if root_prefix.is_empty() {
        String::new()
    } else {
        format!("{root_prefix}/")
    };
    let mut external_index = index_path.map(load_external_hash);
    let mut index_changed = false;
    let mut entries = Vec::new();

    while let Some(item) = stream.next().await {
        let meta = item.map_err(CrabError::Storage)?;
        let key = meta.location.as_ref();
        let rel = if root_prefix.is_empty() {
            key
        } else if let Some(rel) = key.strip_prefix(&root_child_prefix) {
            rel
        } else {
            continue;
        };
        if rel.is_empty() || rel.ends_with('/') {
            continue;
        }

        let member_locator = if locator.is_empty() {
            rel.to_owned()
        } else {
            format!("{}/{}", locator.trim_end_matches('/'), rel)
        };
        let validator = object_store_validator(&meta);
        let cached_hash = external_index.as_ref().and_then(|index| {
            validator.as_deref().and_then(|validator| {
                index
                    .reusable(
                        provider,
                        &member_locator,
                        credential_scope,
                        meta.size,
                        Some(validator),
                    )
                    .and_then(parse_cached_hash)
            })
        });
        let file_hash = if let Some(hash) = cached_hash {
            hash
        } else {
            let result = store
                .get(&meta.location)
                .await
                .map_err(CrabError::Storage)?;
            hash_object_store_result(result).await?
        };
        if cached_hash.is_none()
            && let (Some(index), Some(validator)) = (external_index.as_mut(), validator)
            && index
                .insert(ExternalHashRecord {
                    provider: provider.to_owned(),
                    locator: member_locator,
                    credential_scope: credential_scope.to_owned(),
                    size: meta.size,
                    validator: Some(validator),
                    last_modified: Some(meta.last_modified.to_rfc3339()),
                    crab_hash: format!("b3:{}", hex_bytes(&file_hash)),
                    observed_at_unix_ms: now_unix_ms(),
                })
                .is_ok()
        {
            index_changed = true;
        }
        entries.push(crate::hasher::TreeEntry {
            path: PathBuf::from(rel),
            kind: crate::hasher::TreeEntryKind::File,
            file_hash,
            size: meta.size,
            mode: 0o644,
        });
    }

    if entries.is_empty() {
        return Err(CrabError::Storage(object_store::Error::NotFound {
            path: location.to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "external URL dependency not found",
            )),
        }));
    }

    entries.sort_by(|left, right| left.path.cmp(&right.path));

    if index_changed
        && let (Some(index), Some(index_path)) = (external_index, index_path)
        && let Err(error) = index.save_atomic(index_path)
    {
        tracing::warn!(path = %index_path.display(), error = %error, "could not persist external hash index");
    }

    Ok(crate::hasher::hash_tree_entries(&entries))
}

fn load_external_hash(path: &Path) -> ExternalHashIndex {
    match ExternalHashIndex::load(path) {
        Ok(index) => index,
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "ignoring invalid external hash index");
            ExternalHashIndex::default()
        }
    }
}

fn record_external_hash(
    index_path: Option<&Path>,
    provider: &str,
    locator: &str,
    credential_scope: &str,
    size: u64,
    validator: String,
    last_modified: Option<String>,
    hash: [u8; 32],
) {
    let Some(index_path) = index_path else {
        return;
    };
    let mut index = load_external_hash(index_path);
    if index
        .insert(ExternalHashRecord {
            provider: provider.to_owned(),
            locator: locator.to_owned(),
            credential_scope: credential_scope.to_owned(),
            size,
            validator: Some(validator),
            last_modified,
            crab_hash: format!("b3:{}", hex_bytes(&hash)),
            observed_at_unix_ms: now_unix_ms(),
        })
        .is_ok()
        && let Err(error) = index.save_atomic(index_path)
    {
        tracing::warn!(path = %index_path.display(), error = %error, "could not persist external hash index");
    }
}

fn object_store_validator(meta: &object_store::ObjectMeta) -> Option<String> {
    meta.version.clone().or_else(|| {
        meta.e_tag
            .clone()
            .filter(|value| !value.trim_start().starts_with("W/"))
    })
}

fn redacted_object_store_locator(parsed: &url::Url) -> String {
    let mut parsed = parsed.clone();
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

fn object_store_scope(parsed: &url::Url) -> String {
    let endpoint = parsed.host_str().unwrap_or_default();
    let profile = std::env::var("AWS_PROFILE")
        .ok()
        .filter(|value| !value.trim().is_empty());
    profile.map_or_else(
        || "unscoped".to_owned(),
        |profile| format!("profile:{profile}@{endpoint}"),
    )
}

async fn hash_object_store_result(result: object_store::GetResult) -> Result<[u8; 32]> {
    let mut stream = result.into_stream();
    let mut hasher = blake3::Hasher::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(CrabError::Storage)?;
        hasher.update(&chunk);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn is_object_store_not_found(err: &object_store::Error) -> bool {
    matches!(err, object_store::Error::NotFound { .. })
}

#[cfg(any(test, feature = "testing"))]
pub mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serves one HTTP response containing `body`.
    pub fn serve_http_body_once(body: &'static [u8]) -> String {
        serve_http_body_n(body, 1)
    }

    /// Serves `requests` HTTP responses containing `body`.
    pub fn serve_http_body_n(body: &'static [u8], requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        format!("http://{addr}/data.bin")
    }

    /// Serves HEAD/GET responses with one strong ETag and returns a request count.
    pub fn serve_http_body_with_etag(
        body: &'static [u8],
        requests: usize,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let thread_count = Arc::clone(&count);
        std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let read = stream.read(&mut request).unwrap_or(0);
                thread_count.fetch_add(1, Ordering::SeqCst);
                let is_head = request[..read].starts_with(b"HEAD ");
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                if !is_head {
                    stream.write_all(body).unwrap();
                }
            }
        });
        (format!("http://{addr}/data.bin"), count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn url_hash_accepts_pinned_b3_digest() {
        let dep = Dep::Url {
            url: "https://example.com/data.bin".to_owned(),
            digest: Some(format!("b3:{}", "ab".repeat(32))),
        };
        let (key, digest) = dep.url_hash().unwrap().unwrap();
        assert_eq!(key, "https://example.com/data.bin");
        assert_eq!(digest, [0xab; 32]);
    }

    #[test]
    fn url_hash_rejects_non_b3_digest() {
        let sha = Dep::Url {
            url: "https://example.com/data.bin".to_owned(),
            digest: Some(format!("sha256:{}", "ab".repeat(32))),
        };
        assert!(matches!(
            sha.url_hash().unwrap_err(),
            CrabError::Configuration { .. }
        ));
    }

    #[test]
    fn url_hash_fetches_unpinned_http_body() {
        let url = test_support::serve_http_body_once(b"live-url-body");
        let dep = Dep::Url {
            url: url.clone(),
            digest: None,
        };

        let (key, digest) = dep.url_hash().unwrap().unwrap();
        assert_eq!(key, url);
        assert_eq!(digest, *blake3::hash(b"live-url-body").as_bytes());
    }

    #[test]
    fn url_hash_reuses_validator_index_without_get_body() {
        let (url, requests) = test_support::serve_http_body_with_etag(b"indexed", 3);
        let temp = tempfile::tempdir().unwrap();
        let index_path = temp.path().join(".crab/workflow/external-hashes.json");
        let dep = Dep::Url { url, digest: None };
        let first = dep
            .url_hash_with_remote_aliases_and_index(&BTreeMap::new(), Some(&index_path))
            .unwrap()
            .unwrap()
            .1;
        let second = dep
            .url_hash_with_remote_aliases_and_index(&BTreeMap::new(), Some(&index_path))
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(first, second);
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn url_hash_expands_remote_alias_to_backing_url() {
        let base_url = test_support::serve_http_body_once(b"remote-alias-body");
        let base_url = base_url.trim_end_matches("data.bin").to_owned();
        let dep = Dep::Url {
            url: "remote://datasets/raw.csv".to_owned(),
            digest: None,
        };
        let aliases = BTreeMap::from([("datasets".to_owned(), base_url)]);

        let (key, digest) = dep.url_hash_with_remote_aliases(&aliases).unwrap().unwrap();

        assert_eq!(key, "remote://datasets/raw.csv");
        assert_eq!(digest, *blake3::hash(b"remote-alias-body").as_bytes());
    }

    #[test]
    fn url_hash_reports_missing_remote_alias_config() {
        let dep = Dep::Url {
            url: "remote://datasets/raw.csv".to_owned(),
            digest: None,
        };

        match dep
            .url_hash_with_remote_aliases(&BTreeMap::new())
            .unwrap_err()
        {
            CrabError::Configuration { key, origin } => {
                assert_eq!(key, "workflow.remotes.datasets");
                assert!(origin.contains("remote://datasets/raw.csv"));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn url_hash_expands_remote_alias_to_absolute_local_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("raw.csv"), b"local-alias-body").unwrap();
        let dep = Dep::Url {
            url: "remote://datasets/raw.csv".to_owned(),
            digest: None,
        };
        let aliases = BTreeMap::from([(
            "datasets".to_owned(),
            tmp.path().to_string_lossy().into_owned(),
        )]);

        let (key, digest) = dep.url_hash_with_remote_aliases(&aliases).unwrap().unwrap();

        assert_eq!(key, "remote://datasets/raw.csv");
        assert_eq!(digest, *blake3::hash(b"local-alias-body").as_bytes());
    }

    #[test]
    fn url_hash_hashes_file_url_body() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("data.bin");
        std::fs::write(&path, b"file-url-body").unwrap();
        let url = url::Url::from_file_path(&path).unwrap().to_string();
        let dep = Dep::Url {
            url: url.clone(),
            digest: None,
        };

        let (key, digest) = dep.url_hash().unwrap().unwrap();
        assert_eq!(key, url);
        assert_eq!(digest, *blake3::hash(b"file-url-body").as_bytes());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn object_store_url_hash_hashes_object_body() {
        let store = object_store::memory::InMemory::new();
        let location = ObjectPath::from("data.bin");
        store
            .put(&location, object_store::PutPayload::from("object-body"))
            .await
            .unwrap();

        let digest = hash_object_store_location(
            &store,
            &location,
            None,
            "memory",
            "memory://data.bin",
            "test",
        )
        .await
        .expect("object store object should hash");

        assert_eq!(digest, *blake3::hash(b"object-body").as_bytes());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn object_store_url_hash_hashes_prefix_manifest() {
        let store = object_store::memory::InMemory::new();
        store
            .put(
                &ObjectPath::from("dataset/a.txt"),
                object_store::PutPayload::from("a"),
            )
            .await
            .unwrap();
        store
            .put(
                &ObjectPath::from("dataset/nested/b.txt"),
                object_store::PutPayload::from("b"),
            )
            .await
            .unwrap();
        store
            .put(
                &ObjectPath::from("dataset-neighbor.txt"),
                object_store::PutPayload::from("nope"),
            )
            .await
            .unwrap();
        let expected = crate::hasher::hash_tree_entries(&[
            crate::hasher::TreeEntry {
                path: PathBuf::from("a.txt"),
                kind: crate::hasher::TreeEntryKind::File,
                file_hash: *blake3::hash(b"a").as_bytes(),
                size: 1,
                mode: 0o644,
            },
            crate::hasher::TreeEntry {
                path: PathBuf::from("nested/b.txt"),
                kind: crate::hasher::TreeEntryKind::File,
                file_hash: *blake3::hash(b"b").as_bytes(),
                size: 1,
                mode: 0o644,
            },
        ]);

        let digest = hash_object_store_location(
            &store,
            &ObjectPath::from("dataset"),
            None,
            "memory",
            "memory://dataset",
            "test",
        )
        .await
        .expect("object store prefix should hash");

        assert_eq!(digest, expected);
    }

    #[test]
    fn url_hash_rejects_unpinned_non_http_scheme() {
        for scheme in [
            "ssh", "sftp", "hdfs", "webhdfs", "webdav", "webdavs", "gdrive", "oss",
        ] {
            let dep = Dep::Url {
                url: format!("{scheme}://example.com/data.bin"),
                digest: None,
            };
            assert!(matches!(
                dep.url_hash().unwrap_err(),
                CrabError::StageRemoteExecutionUnsupported
            ));
        }
    }

    #[test]
    fn provider_validation_rejects_unsupported_scheme_before_execution() {
        let error = validate_url_provider(
            "sftp://example.com/data.bin",
            &BTreeMap::new(),
            "stage 'train' dependency",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CrabError::Configuration { key, .. }
                if key == "workflow.external_provider_unsupported"
        ));
        validate_url_provider(
            "https://example.com/data.bin",
            &BTreeMap::new(),
            "dependency",
        )
        .unwrap();
        let error = validate_url_provider(
            "https://example.com/data.bin?token=secret",
            &BTreeMap::new(),
            "dependency",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CrabError::Configuration { key, .. }
                if key == "workflow.external_provider_credentials"
        ));
    }

    #[test]
    fn provider_errors_do_not_echo_remote_alias_credentials() {
        let error = expand_remote_alias_url(
            "remote://user:secret@example/path",
            &BTreeMap::new(),
            "dependency",
        )
        .expect_err("credential-bearing aliases must fail closed");
        let rendered = error.to_string();
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("user"));
    }
}
