//! Copy Git LFS payloads referenced by imported pointer files.
//!
//! Objects stream from the source's batch API into Crab's LFS store, which
//! verifies each SHA-256 and size before completing the upload. Imported refs
//! become visible only after every pointer has a verified payload.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crab_git::pointer_detect::PointerKind;
use crab_git::receive_plan::PointerDependency;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::ImportError;
use crate::repository::Repository;

const MEDIA_TYPE: &str = "application/vnd.git-lfs+json";
const BATCH_OBJECTS: usize = 100;
const PARALLEL_DOWNLOADS: usize = 4;
const MAX_BATCH_RESPONSE: usize = 16 * 1024 * 1024;

#[derive(Serialize)]
struct BatchRequest<'a> {
    operation: &'static str,
    transfers: [&'static str; 1],
    objects: &'a [BatchObject],
    hash_algo: &'static str,
}

#[derive(Serialize, Deserialize, Clone)]
struct BatchObject {
    oid: String,
    size: u64,
}

#[derive(Deserialize)]
struct BatchResponse {
    objects: Vec<ResponseObject>,
}

#[derive(Deserialize)]
struct ResponseObject {
    oid: String,
    size: u64,
    #[serde(default)]
    actions: Option<Actions>,
    #[serde(default)]
    error: Option<ObjectError>,
}

#[derive(Deserialize)]
struct Actions {
    download: Option<Action>,
}

#[derive(Deserialize)]
struct Action {
    href: String,
    #[serde(default)]
    header: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct ObjectError {
    code: u16,
    message: String,
}

/// Copy every LFS payload named by `pointers`, then prove the complete set.
///
/// Payloads already in the repository's LFS store are not downloaded again.
pub(super) async fn copy(
    source: &str,
    token: Option<&str>,
    repository: &Repository,
    pointers: &[PointerDependency],
    cancel: &CancellationToken,
) -> Result<(), ImportError> {
    let wanted = wanted(pointers.iter().map(|dependency| &dependency.pointer))?;
    if wanted.is_empty() {
        return Ok(());
    }
    let copied = download_missing(source, token, repository, &wanted, cancel).await?;
    crab_read::dependency_proof::verify_capsule_dependencies_except_prepared(
        &repository.layout,
        &crab_metadata::capsule_protocol::PointerCatalog::new(),
        pointers,
        &BTreeSet::new(),
        &copied,
        proof_limits(pointers.len()),
        cancel,
    )
    .await
    .map_err(|error| ImportError::Publication(Box::new(error)))
}

fn wanted<'a>(
    pointers: impl Iterator<Item = &'a PointerKind>,
) -> Result<BTreeMap<[u8; 32], u64>, ImportError> {
    let mut wanted = BTreeMap::new();
    for pointer in pointers {
        match pointer {
            PointerKind::Crab(_) => {
                return Err(ImportError::Git(
                    "The Git source contains Crab pointer files, which another host cannot supply"
                        .into(),
                ));
            }
            PointerKind::Lfs(pointer) => {
                if wanted
                    .insert(pointer.oid, pointer.size)
                    .is_some_and(|size| size != pointer.size)
                {
                    return Err(ImportError::Git(
                        "The Git source names one LFS object with two sizes".into(),
                    ));
                }
            }
            PointerKind::NotAPointer => {}
        }
    }
    Ok(wanted)
}

/// Download payloads the repository lacks; returns the newly stored objects.
async fn download_missing(
    source: &str,
    token: Option<&str>,
    repository: &Repository,
    wanted: &BTreeMap<[u8; 32], u64>,
    cancel: &CancellationToken,
) -> Result<BTreeSet<[u8; 32]>, ImportError> {
    let store = crab_lfs::LfsObjectStore::new(
        repository.layout.store().clone(),
        repository.layout.repo_prefix(),
    );
    let mut missing = Vec::new();
    for (oid, size) in wanted {
        if !store.exists(oid).await.map_err(lfs)? {
            missing.push(BatchObject {
                oid: hex(oid),
                size: *size,
            });
        }
    }
    let mut copied = BTreeSet::new();
    if missing.is_empty() {
        return Ok(copied);
    }
    let client = client()?;
    for batch in missing.chunks(BATCH_OBJECTS) {
        if cancel.is_cancelled() {
            return Err(ImportError::Cancelled);
        }
        let actions = resolve(&client, source, token, batch).await?;
        let results = futures_util::stream::iter(actions.into_iter().map(|(object, action)| {
            let (client, store) = (&client, &store);
            async move { download(client, store, object, action).await }
        }))
        .buffer_unordered(PARALLEL_DOWNLOADS)
        .try_collect::<Vec<_>>()
        .await?;
        copied.extend(results);
    }
    Ok(copied)
}

async fn resolve(
    client: &reqwest::Client,
    source: &str,
    token: Option<&str>,
    batch: &[BatchObject],
) -> Result<Vec<(BatchObject, Action)>, ImportError> {
    let mut request = client
        .post(format!(
            "{}/info/lfs/objects/batch",
            source.trim_end_matches('/')
        ))
        .header(ACCEPT, MEDIA_TYPE)
        .header(CONTENT_TYPE, MEDIA_TYPE)
        .json(&BatchRequest {
            operation: "download",
            transfers: ["basic"],
            objects: batch,
            hash_algo: "sha256",
        });
    if let Some(token) = token {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ImportError::Invalid("the Git source token is invalid"))?;
        value.set_sensitive(true);
        request = request.header(AUTHORIZATION, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ImportError::Git(format!("Git LFS batch request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(ImportError::Git(format!(
            "The Git LFS server returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| ImportError::Git(format!("Git LFS batch read failed: {error}")))?;
        if body.len() + chunk.len() > MAX_BATCH_RESPONSE {
            return Err(ImportError::Git(
                "The Git LFS batch response is too large".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let response: BatchResponse = serde_json::from_slice(&body).map_err(|_| {
        ImportError::Git("The Git LFS server sent an invalid batch response".into())
    })?;
    let requested = batch
        .iter()
        .map(|object| (object.oid.as_str(), object.size))
        .collect::<BTreeMap<_, _>>();
    let mut actions = Vec::with_capacity(batch.len());
    for object in response.objects {
        if requested.get(object.oid.as_str()) != Some(&object.size) {
            return Err(ImportError::Git(
                "The Git LFS server answered for an object that was not requested".into(),
            ));
        }
        if let Some(error) = object.error {
            return Err(ImportError::Git(format!(
                "The Git LFS server cannot provide {} ({}: {})",
                object.oid, error.code, error.message
            )));
        }
        let action = object
            .actions
            .and_then(|actions| actions.download)
            .ok_or_else(|| {
                ImportError::Git(format!(
                    "The Git LFS server has no download for {}",
                    object.oid
                ))
            })?;
        actions.push((
            BatchObject {
                oid: object.oid,
                size: object.size,
            },
            action,
        ));
    }
    if actions.len() != batch.len() {
        return Err(ImportError::Git(
            "The Git LFS batch response omitted requested objects".into(),
        ));
    }
    Ok(actions)
}

async fn download(
    client: &reqwest::Client,
    store: &crab_lfs::LfsObjectStore,
    object: BatchObject,
    action: Action,
) -> Result<[u8; 32], ImportError> {
    let href = url::Url::parse(&action.href)
        .map_err(|_| ImportError::Git("The Git LFS server returned an invalid download".into()))?;
    if href.scheme() != "https" && !super::source::loopback_http(&href) {
        return Err(ImportError::Git(
            "The Git LFS server returned a non-HTTPS download".into(),
        ));
    }
    let mut headers = HeaderMap::new();
    for (name, value) in action.header {
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| ImportError::Git("The Git LFS download header is invalid".into()))?;
        value.set_sensitive(true);
        headers.insert(
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ImportError::Git("The Git LFS download header is invalid".into()))?,
            value,
        );
    }
    let response = client
        .get(href)
        .headers(headers)
        .send()
        .await
        .map_err(|error| ImportError::Git(format!("Git LFS download failed: {error}")))?;
    if !response.status().is_success() {
        return Err(ImportError::Git(format!(
            "Git LFS download of {} returned HTTP {}",
            object.oid,
            response.status().as_u16()
        )));
    }
    let oid = parse_oid(&object.oid)?;
    let stream = response
        .bytes_stream()
        .map_err(|error| crab_lfs::LfsError::Io {
            source: std::io::Error::other(error),
        });
    store
        .put_byte_stream_with_size(&oid, object.size, stream)
        .await
        .map_err(lfs)?;
    Ok(oid)
}

fn client() -> Result<reqwest::Client, ImportError> {
    reqwest::Client::builder()
        .user_agent("git-lfs/crab-server")
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        // Storage providers redirect downloads; never follow a downgrade.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|error| ImportError::Git(format!("Git LFS client setup failed: {error}")))
}

fn proof_limits(dependencies: usize) -> crab_read::dependency_proof::DependencyProofLimits {
    crab_read::dependency_proof::DependencyProofLimits {
        max_dependencies: dependencies.max(1),
        max_total_file_bytes: u64::MAX,
        max_duration: Duration::from_secs(60 * 60),
        lookup: crab_metadata::file_index_lookup::FileIndexLookupLimits {
            max_files: 1,
            max_shard_visits: 1,
            max_shard_bytes: 1,
            max_recipe_entries: 1,
        },
        content: crab_read::pointer_proof::PointerProofLimits {
            max_file_bytes: u64::MAX,
            max_shard_bytes: 1,
            max_xorb_bytes: 1,
            max_read_bytes: u64::MAX,
            max_chunks: 1,
            max_duration: Duration::from_secs(60 * 60),
        },
    }
}

fn parse_oid(value: &str) -> Result<[u8; 32], ImportError> {
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(value.get(index..index + 2).unwrap_or("zz"), 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ImportError::Git("The Git LFS server returned an invalid object ID".into()))?;
    bytes
        .try_into()
        .map_err(|_| ImportError::Git("The Git LFS server returned an invalid object ID".into()))
}

fn hex(oid: &[u8; 32]) -> String {
    oid.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn lfs(error: crab_lfs::LfsError) -> ImportError {
    ImportError::Publication(Box::new(error))
}
