use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use crab_storage::ETag;
use serde::{Deserialize, Serialize};

use crate::{attributes::PutAttributes, gateway::Repository};

const VERSION: u32 = 2;
const SLOT_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SLOT_RECORD_BYTES: u64 = 4 * 1024;
const MAX_PARTS: usize = 10_000;
const MAX_CAPACITY_SLOTS: usize = 10_000;
const CATALOG_READ_CONCURRENCY: usize = 32;
// One admitted high-fanout part burst should converge in the gateway. Conflicts
// beyond this bound remain retryable across independently scaled instances.
const MAX_STATE_UPDATE_ATTEMPTS: usize = 64;
const MAX_STATE_RETRY_EXPONENT: usize = 5;
const COMPLETION_PLAN_CONTEXT: &str = "crab s3 multipart completion plan v1";

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("multipart upload does not exist")]
    NoSuchUpload,
    #[error("multipart upload is no longer open")]
    NotOpen,
    #[error("multipart upload identity does not match the request")]
    Identity,
    #[error("multipart part number is invalid")]
    PartNumber,
    #[error("multipart part is missing or has a different ETag")]
    InvalidPart,
    #[error("multipart parts are not strictly ascending")]
    InvalidPartOrder,
    #[error("a non-final multipart part is smaller than 5 MiB")]
    EntityTooSmall,
    #[error("the completed multipart object exceeds the S3 object limit")]
    EntityTooLarge,
    #[error("multipart staging capacity is exhausted")]
    Capacity,
    #[error("multipart state changed concurrently")]
    Conflict,
    #[error("multipart state update was cancelled")]
    Cancelled,
    #[error("multipart record is corrupt")]
    Decode(#[from] serde_json::Error),
    #[error("multipart storage failed")]
    Storage(#[from] crab_storage::StorageError),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Open,
    Completing,
    Completed,
    Aborted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Part {
    pub(crate) number: i32,
    pub(crate) etag: String,
    pub(crate) size: u64,
    pub(crate) modified_seconds: u64,
    #[serde(
        default,
        skip_serializing_if = "crate::attributes::Checksums::is_empty"
    )]
    pub(crate) checksums: crate::attributes::Checksums,
    path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Session {
    version: u32,
    pub(crate) id: String,
    pub(crate) bucket: String,
    pub(crate) key: String,
    pub(crate) branch: String,
    pub(crate) path: String,
    pub(crate) principal: String,
    pub(crate) created_seconds: u64,
    expires_seconds: u64,
    max_staged_bytes: u64,
    capacity_slot: u32,
    capacity_generation: u64,
    revision: u64,
    state: State,
    pub(crate) attributes: PutAttributes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checksum_algorithm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checksum_type: Option<String>,
    pub(crate) parts: BTreeMap<i32, Part>,
    selected_parts: Option<Vec<(i32, String)>>,
    completion_etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completion_checksums: Option<crate::attributes::Checksums>,
}

pub(crate) struct Loaded {
    pub(crate) session: Session,
    etag: ETag,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityOwner {
    upload_id: String,
    created_seconds: u64,
    expires_seconds: u64,
}

// Slots are released by CAS to an empty owner, never deleted. The generation
// fences a delayed release after another upload reuses it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacitySlot {
    version: u32,
    number: u32,
    generation: u64,
    owner: Option<CapacityOwner>,
}

struct LoadedCapacitySlot {
    slot: CapacitySlot,
    etag: ETag,
}

struct CapacityReservation {
    number: u32,
    generation: u64,
}

pub(crate) struct Initiation<'a> {
    pub(crate) bucket: &'a str,
    pub(crate) key: &'a str,
    pub(crate) branch: &'a str,
    pub(crate) path: &'a str,
    pub(crate) principal: &'a str,
    pub(crate) attributes: PutAttributes,
    pub(crate) checksum_algorithm: Option<String>,
    pub(crate) checksum_type: Option<String>,
    pub(crate) now: u64,
}

pub(crate) fn publication_plan_id(upload_id: &str) -> String {
    blake3::Hash::from_bytes(blake3::derive_key(
        COMPLETION_PLAN_CONTEXT,
        upload_id.as_bytes(),
    ))
    .to_hex()
    .to_string()
}

pub(crate) async fn create(repository: &Repository, initiation: Initiation<'_>) -> Result<Session> {
    for _ in 0..8 {
        let id = ulid::Ulid::new().to_string();
        let expires_seconds = initiation
            .now
            .checked_add(repository.config.multipart_upload_ttl_seconds)
            .ok_or(Error::Capacity)?;
        let reservation =
            acquire_capacity(repository, &id, initiation.now, expires_seconds).await?;
        let session = Session {
            version: VERSION,
            id: id.clone(),
            bucket: initiation.bucket.to_owned(),
            key: initiation.key.to_owned(),
            branch: initiation.branch.to_owned(),
            path: initiation.path.to_owned(),
            principal: initiation.principal.to_owned(),
            created_seconds: initiation.now,
            expires_seconds,
            max_staged_bytes: repository.config.multipart_staging_bytes_per_upload,
            capacity_slot: reservation.number,
            capacity_generation: reservation.generation,
            revision: 0,
            state: State::Open,
            attributes: initiation.attributes.clone(),
            checksum_algorithm: initiation.checksum_algorithm.clone(),
            checksum_type: initiation.checksum_type.clone(),
            parts: BTreeMap::new(),
            selected_parts: None,
            completion_etag: None,
            completion_checksums: None,
        };
        let bytes = serde_json::to_vec(&session)?;
        match repository
            .store
            .create_strict(&state_path(repository, &id), Bytes::from(bytes))
            .await
        {
            Ok(()) => return Ok(session),
            Err(crab_storage::StorageError::StateConflict { .. }) => {
                if load(repository, &id)
                    .await
                    .is_ok_and(|loaded| session_identity_matches(&loaded.session, &session))
                {
                    return Ok(session);
                }
                release_capacity(repository, &id, &reservation).await?;
            }
            Err(error) => {
                let created = load(repository, &id)
                    .await
                    .is_ok_and(|loaded| session_identity_matches(&loaded.session, &session));
                if created {
                    return Ok(session);
                }
                release_capacity(repository, &id, &reservation).await?;
                return Err(error.into());
            }
        }
    }
    Err(Error::Conflict)
}

fn session_identity_matches(actual: &Session, expected: &Session) -> bool {
    actual.version == expected.version
        && actual.id == expected.id
        && actual.bucket == expected.bucket
        && actual.key == expected.key
        && actual.branch == expected.branch
        && actual.path == expected.path
        && actual.principal == expected.principal
        && actual.created_seconds == expected.created_seconds
        && actual.expires_seconds == expected.expires_seconds
        && actual.capacity_slot == expected.capacity_slot
        && actual.capacity_generation == expected.capacity_generation
}

pub(crate) async fn load(repository: &Repository, id: &str) -> Result<Loaded> {
    validate_id(id)?;
    let (bytes, etag) = repository
        .store
        .get_with_etag_bounded(&state_path(repository, id), MAX_RECORD_BYTES)
        .await
        .map_err(|error| match error {
            crab_storage::StorageError::NotFound { .. } => Error::NoSuchUpload,
            error => Error::Storage(error),
        })?;
    let session: Session = serde_json::from_slice(&bytes)?;
    if session.version != VERSION || session.id != id {
        return Err(Error::Identity);
    }
    Ok(Loaded { session, etag })
}

pub(crate) async fn load_open(repository: &Repository, id: &str, now: u64) -> Result<Loaded> {
    let loaded = load(repository, id).await?;
    if matches!(loaded.session.state, State::Open) && now >= loaded.session.expires_seconds {
        abort(repository, loaded).await?;
        return Err(Error::NotOpen);
    }
    if matches!(loaded.session.state, State::Open) {
        Ok(loaded)
    } else {
        Err(Error::NotOpen)
    }
}

pub(crate) async fn load_completion(repository: &Repository, id: &str, now: u64) -> Result<Loaded> {
    let loaded = load(repository, id).await?;
    if matches!(loaded.session.state, State::Open) && now >= loaded.session.expires_seconds {
        abort(repository, loaded).await?;
        return Err(Error::NotOpen);
    }
    if matches!(loaded.session.state, State::Aborted) {
        Err(Error::NotOpen)
    } else {
        Ok(loaded)
    }
}

async fn acquire_capacity(
    repository: &Repository,
    upload_id: &str,
    created_seconds: u64,
    expires_seconds: u64,
) -> Result<CapacityReservation> {
    let count = repository.config.max_active_multipart_uploads;
    let start = upload_id.bytes().fold(0_usize, |value, byte| {
        value.rotate_left(5) ^ usize::from(byte)
    }) % count;
    for offset in 0..count {
        let number = u32::try_from((start + offset) % count).map_err(|_| Error::Capacity)?;
        let path = capacity_path(repository, number);
        let owner = CapacityOwner {
            upload_id: upload_id.to_owned(),
            created_seconds,
            expires_seconds,
        };
        let initial = CapacitySlot {
            version: SLOT_VERSION,
            number,
            generation: 0,
            owner: Some(owner.clone()),
        };
        let initial_bytes = Bytes::from(serde_json::to_vec(&initial)?);
        match repository.store.create_strict(&path, initial_bytes).await {
            Ok(()) => {
                return Ok(CapacityReservation {
                    number,
                    generation: 0,
                });
            }
            Err(crab_storage::StorageError::StateConflict { .. }) => {}
            Err(error) => {
                let loaded = load_capacity_path(repository, &path).await;
                if loaded
                    .as_ref()
                    .is_ok_and(|loaded| loaded.slot.owner.as_ref() == Some(&owner))
                {
                    return Ok(CapacityReservation {
                        number,
                        generation: 0,
                    });
                }
                return Err(error.into());
            }
        }
        let loaded = load_capacity_path(repository, &path).await?;
        if loaded.slot.generation == 0 && loaded.slot.owner.as_ref() == Some(&owner) {
            return Ok(CapacityReservation {
                number,
                generation: 0,
            });
        }
        if loaded.slot.owner.is_some() {
            continue;
        }
        let generation = loaded.slot.generation.saturating_add(1);
        let replacement = CapacitySlot {
            owner: Some(owner.clone()),
            generation,
            ..loaded.slot
        };
        let bytes = Bytes::from(serde_json::to_vec(&replacement)?);
        match repository.store.update(&path, bytes, loaded.etag).await {
            Ok(_) => return Ok(CapacityReservation { number, generation }),
            Err(crab_storage::StorageError::StateConflict { .. }) => continue,
            Err(error) => {
                let current = load_capacity_path(repository, &path).await;
                if current.as_ref().is_ok_and(|current| {
                    current.slot.generation == generation
                        && current.slot.owner.as_ref() == Some(&owner)
                }) {
                    return Ok(CapacityReservation { number, generation });
                }
                return Err(error.into());
            }
        }
    }
    Err(Error::Capacity)
}

async fn load_capacity_path(
    repository: &Repository,
    path: &object_store::path::Path,
) -> Result<LoadedCapacitySlot> {
    let (bytes, etag) = repository
        .store
        .get_with_etag_bounded(path, MAX_SLOT_RECORD_BYTES)
        .await?;
    let slot: CapacitySlot = serde_json::from_slice(&bytes)?;
    if slot.version != SLOT_VERSION || capacity_path(repository, slot.number) != *path {
        return Err(Error::Identity);
    }
    Ok(LoadedCapacitySlot { slot, etag })
}

async fn release_capacity(
    repository: &Repository,
    upload_id: &str,
    reservation: &CapacityReservation,
) -> Result<()> {
    let path = capacity_path(repository, reservation.number);
    for _ in 0..MAX_STATE_UPDATE_ATTEMPTS {
        let loaded = load_capacity_path(repository, &path).await?;
        let owned = loaded.slot.generation == reservation.generation
            && loaded
                .slot
                .owner
                .as_ref()
                .is_some_and(|owner| owner.upload_id == upload_id);
        if !owned {
            return Ok(());
        }
        let released = CapacitySlot {
            owner: None,
            ..loaded.slot
        };
        let bytes = Bytes::from(serde_json::to_vec(&released)?);
        match repository.store.update(&path, bytes, loaded.etag).await {
            Ok(_) => return Ok(()),
            Err(crab_storage::StorageError::StateConflict { .. }) => continue,
            Err(error) => {
                let current = load_capacity_path(repository, &path).await;
                if current.as_ref().is_ok_and(|current| {
                    current.slot.generation != reservation.generation
                        || current
                            .slot
                            .owner
                            .as_ref()
                            .is_none_or(|owner| owner.upload_id != upload_id)
                }) {
                    return Ok(());
                }
                return Err(error.into());
            }
        }
    }
    Err(Error::Conflict)
}

pub(crate) fn authorize(session: &Session, bucket: &str, key: &str, principal: &str) -> Result<()> {
    if session.bucket != bucket || session.key != key || session.principal != principal {
        return Err(Error::Identity);
    }
    Ok(())
}

fn capacity_path(repository: &Repository, number: u32) -> object_store::path::Path {
    repository
        .layout
        .repo_path(&format!("s3/multipart/capacity/{number:05}.json"))
}

pub(crate) async fn register_part(
    repository: &Repository,
    mut loaded: Loaded,
    number: i32,
    spool: &crate::content::Spool,
    etag: String,
    checksums: crate::attributes::Checksums,
    now: u64,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Part> {
    if !(1..=10_000).contains(&number) {
        return Err(Error::PartNumber);
    }
    if !matches!(loaded.session.state, State::Open) {
        return Err(Error::NotOpen);
    }
    if now >= loaded.session.expires_seconds {
        abort(repository, loaded).await?;
        return Err(Error::NotOpen);
    }
    // A transfer-specific identity lets a losing registration delete only its
    // own payload; content-derived paths let concurrent equal-ETag requests
    // accidentally delete the winner's bytes.
    let path = format!(
        "s3/multipart/parts/{}/{number}/{etag}/{}",
        loaded.session.id,
        ulid::Ulid::new()
    );
    repository
        .store
        .put_multipart_file_retry(
            &repository.layout.repo_path(&path),
            spool.path(),
            spool.size,
            spool.digests.blake3,
            8 * 1024 * 1024,
            cancel,
            None,
        )
        .await?;
    let part = Part {
        number,
        etag,
        size: spool.size,
        modified_seconds: now,
        checksums,
        path,
    };
    for attempt in 0..MAX_STATE_UPDATE_ATTEMPTS {
        if !matches!(loaded.session.state, State::Open) {
            cleanup_unreferenced_part(repository, &loaded.session, &part).await;
            return Err(Error::NotOpen);
        }
        let replaced = loaded.session.parts.insert(number, part.clone());
        if loaded.session.parts.len() > MAX_PARTS {
            cleanup_part_object(repository, &loaded.session.id, &part).await;
            return Err(Error::PartNumber);
        }
        let staged_bytes = loaded
            .session
            .parts
            .values()
            .try_fold(0_u64, |total, registered| {
                total.checked_add(registered.size).ok_or(Error::Capacity)
            });
        let Ok(staged_bytes) = staged_bytes else {
            cleanup_part_object(repository, &loaded.session.id, &part).await;
            return Err(Error::Capacity);
        };
        if staged_bytes > loaded.session.max_staged_bytes {
            cleanup_part_object(repository, &loaded.session.id, &part).await;
            return Err(Error::Capacity);
        }
        loaded.session.revision = loaded.session.revision.saturating_add(1);
        match save(repository, &loaded).await {
            Ok(()) => {
                if let Some(replaced) = replaced {
                    cleanup_unreferenced_part(repository, &loaded.session, &replaced).await;
                }
                return Ok(part);
            }
            Err(Error::Conflict) => {
                if attempt + 1 == MAX_STATE_UPDATE_ATTEMPTS {
                    break;
                }
                if let Err(error) =
                    wait_for_state_retry(&loaded.session.id, number, attempt, cancel).await
                {
                    cleanup_part_object(repository, &loaded.session.id, &part).await;
                    return Err(error);
                }
                loaded = match load(repository, &loaded.session.id).await {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        cleanup_part_object(repository, &loaded.session.id, &part).await;
                        return Err(error);
                    }
                };
            }
            Err(error) => {
                let Ok(current) = load(repository, &loaded.session.id).await else {
                    // The update outcome is ambiguous while its state cannot be
                    // read back. Retain the unique payload for later recovery.
                    return Err(error);
                };
                if current
                    .session
                    .parts
                    .get(&number)
                    .is_some_and(|registered| registered.path == part.path)
                {
                    if let Some(replaced) = replaced {
                        cleanup_unreferenced_part(repository, &current.session, &replaced).await;
                    }
                    return Ok(part);
                }
                cleanup_unreferenced_part(repository, &current.session, &part).await;
                return Err(error);
            }
        }
    }
    cleanup_part_object(repository, &loaded.session.id, &part).await;
    Err(Error::Conflict)
}

async fn cleanup_unreferenced_part(repository: &Repository, session: &Session, part: &Part) {
    if session
        .parts
        .values()
        .any(|registered| registered.path == part.path)
    {
        return;
    }
    cleanup_part_object(repository, &session.id, part).await;
}

async fn cleanup_part_object(repository: &Repository, upload_id: &str, part: &Part) {
    let path = repository.layout.repo_path(&part.path);
    match repository.store.delete(&path).await {
        Ok(()) | Err(crab_storage::StorageError::NotFound { .. }) => {}
        Err(error) => {
            tracing::warn!(
                %upload_id,
                part_number = part.number,
                %error,
                "multipart orphan cleanup failed"
            );
        }
    }
}

async fn wait_for_state_retry(
    upload_id: &str,
    part_number: i32,
    attempt: usize,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let base_ms = 1_u64 << attempt.min(MAX_STATE_RETRY_EXPONENT);
    let seed = upload_id.bytes().fold(part_number as u64, |value, byte| {
        value.rotate_left(5) ^ u64::from(byte)
    });
    let jitter_ms = seed.wrapping_add(attempt as u64) % (base_ms + 1);
    tokio::select! {
        () = cancel.cancelled() => Err(Error::Cancelled),
        () = tokio::time::sleep(Duration::from_millis(base_ms + jitter_ms)) => Ok(()),
    }
}

pub(crate) async fn part_stream(
    repository: &Repository,
    part: &Part,
) -> Result<
    impl futures_util::Stream<Item = std::result::Result<Bytes, crab_storage::StorageError>>
    + Send
    + 'static,
> {
    let (metadata, range, stream) = repository
        .store
        .get_stream(&repository.layout.repo_path(&part.path), None)
        .await?;
    if metadata.size != part.size || range != (0..part.size) {
        return Err(Error::InvalidPart);
    }
    Ok(stream)
}

pub(crate) fn parts_stream<'a>(
    repository: &'a Repository,
    parts: &'a [Part],
) -> impl futures_util::Stream<Item = Result<Bytes>> + Send + 'a {
    use futures_util::{StreamExt as _, TryStreamExt as _};

    futures_util::stream::iter(parts)
        .then(move |part| part_stream(repository, part))
        .map_ok(|stream| stream.map_err(Error::Storage))
        .try_flatten()
}

pub(crate) async fn freeze(
    repository: &Repository,
    mut loaded: Loaded,
    selected: &[(i32, String)],
    max_total_bytes: u64,
) -> Result<(Session, Vec<Part>)> {
    if matches!(loaded.session.state, State::Completing) {
        if loaded.session.selected_parts.as_deref() != Some(selected) {
            return Err(Error::InvalidPart);
        }
        let parts = selected
            .iter()
            .map(|(number, _)| {
                loaded
                    .session
                    .parts
                    .get(number)
                    .cloned()
                    .ok_or(Error::InvalidPart)
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok((loaded.session, parts));
    }
    if !matches!(loaded.session.state, State::Open) {
        return Err(Error::NotOpen);
    }
    if selected.is_empty() || selected.len() > MAX_PARTS {
        return Err(Error::InvalidPart);
    }
    let mut previous = 0;
    let mut total_bytes = 0_u64;
    let mut parts = Vec::with_capacity(selected.len());
    for (index, (number, etag)) in selected.iter().enumerate() {
        if *number <= previous {
            return Err(Error::InvalidPartOrder);
        }
        previous = *number;
        let part = loaded.session.parts.get(number).ok_or(Error::InvalidPart)?;
        if &part.etag != etag {
            return Err(Error::InvalidPart);
        }
        if index + 1 != selected.len() && part.size < 5 * 1024 * 1024 {
            return Err(Error::EntityTooSmall);
        }
        total_bytes = total_bytes
            .checked_add(part.size)
            .ok_or(Error::EntityTooLarge)?;
        if total_bytes > max_total_bytes {
            return Err(Error::EntityTooLarge);
        }
        parts.push(part.clone());
    }
    loaded.session.state = State::Completing;
    loaded.session.selected_parts = Some(selected.to_vec());
    loaded.session.revision = loaded.session.revision.saturating_add(1);
    save(repository, &loaded).await?;
    Ok((loaded.session, parts))
}

#[cfg(test)]
pub(crate) async fn part_bytes(repository: &Repository, part: &Part) -> Result<Bytes> {
    let (bytes, _) = repository
        .store
        .get_with_etag_bounded(&repository.layout.repo_path(&part.path), part.size)
        .await?;
    if bytes.len() as u64 != part.size || crate::gateway::md5_hex(&bytes) != part.etag {
        return Err(Error::InvalidPart);
    }
    Ok(bytes)
}

pub(crate) async fn complete(
    repository: &Repository,
    mut loaded: Loaded,
    etag: String,
    checksums: crate::attributes::Checksums,
) -> Result<()> {
    if matches!(loaded.session.state, State::Completed) {
        return if loaded.session.completion_etag.as_deref() == Some(&etag)
            && loaded.session.completion_checksums.as_ref() == Some(&checksums)
        {
            Ok(())
        } else {
            Err(Error::InvalidPart)
        };
    }
    if !matches!(loaded.session.state, State::Completing) {
        return Err(Error::NotOpen);
    }
    if let Some((planned_etag, planned_checksums)) = planned_completion(&loaded.session)?
        && (planned_etag != etag || planned_checksums != checksums)
    {
        return Err(Error::InvalidPart);
    }
    loaded.session.state = State::Completed;
    loaded.session.completion_etag = Some(etag);
    loaded.session.completion_checksums = Some(checksums);
    loaded.session.revision = loaded.session.revision.saturating_add(1);
    save(repository, &loaded).await?;
    if let Err(error) = cleanup_terminal(repository, &loaded.session).await {
        tracing::warn!(upload_id = %loaded.session.id, %error, "completed multipart cleanup failed");
    }
    Ok(())
}

pub(crate) async fn record_completion_outcome(
    repository: &Repository,
    id: &str,
    etag: &str,
    checksums: &crate::attributes::Checksums,
) -> Result<Session> {
    for _ in 0..MAX_STATE_UPDATE_ATTEMPTS {
        let mut loaded = load(repository, id).await?;
        if !matches!(loaded.session.state, State::Completing | State::Completed) {
            return Err(Error::NotOpen);
        }
        match planned_completion(&loaded.session)? {
            Some((recorded_etag, recorded_checksums))
                if recorded_etag == etag && &recorded_checksums == checksums =>
            {
                return Ok(loaded.session);
            }
            Some(_) => return Err(Error::InvalidPart),
            None if matches!(loaded.session.state, State::Completed) => {
                return Err(Error::InvalidPart);
            }
            None => {}
        }
        loaded.session.completion_etag = Some(etag.to_owned());
        loaded.session.completion_checksums = Some(checksums.clone());
        loaded.session.revision = loaded.session.revision.saturating_add(1);
        match save(repository, &loaded).await {
            Ok(()) => return Ok(loaded.session),
            Err(Error::Conflict) => tokio::task::yield_now().await,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

pub(crate) fn planned_completion(
    session: &Session,
) -> Result<Option<(String, crate::attributes::Checksums)>> {
    match (
        session.completion_etag.as_ref(),
        session.completion_checksums.as_ref(),
    ) {
        (Some(etag), Some(checksums)) => Ok(Some((etag.clone(), checksums.clone()))),
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(Error::InvalidPart),
    }
}

pub(crate) fn completed_etag<'a>(
    session: &'a Session,
    selected: &[(i32, String)],
) -> Result<Option<&'a str>> {
    if !matches!(session.state, State::Completed) {
        return Ok(None);
    }
    if session.selected_parts.as_deref() != Some(selected) {
        return Err(Error::InvalidPart);
    }
    Ok(session.completion_etag.as_deref())
}

pub(crate) fn is_completing(session: &Session) -> bool {
    matches!(session.state, State::Completing)
}

pub(crate) fn frozen_parts(session: &Session) -> Result<Option<Vec<Part>>> {
    if !matches!(session.state, State::Completing) {
        return Ok(None);
    }
    let selected = session.selected_parts.as_ref().ok_or(Error::InvalidPart)?;
    selected
        .iter()
        .map(|(number, etag)| {
            session
                .parts
                .get(number)
                .filter(|part| &part.etag == etag)
                .cloned()
                .ok_or(Error::InvalidPart)
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

pub(crate) async fn abort(repository: &Repository, mut loaded: Loaded) -> Result<()> {
    if !matches!(loaded.session.state, State::Open) {
        return Err(Error::NotOpen);
    }
    loaded.session.state = State::Aborted;
    loaded.session.revision = loaded.session.revision.saturating_add(1);
    save(repository, &loaded).await?;
    if let Err(error) = cleanup_terminal(repository, &loaded.session).await {
        // The durable terminal state prevents future registration. Retaining
        // capacity lets restart maintenance retry physical cleanup safely.
        tracing::warn!(upload_id = %loaded.session.id, %error, "aborted multipart cleanup failed");
    }
    Ok(())
}

async fn cleanup_terminal(repository: &Repository, session: &Session) -> Result<()> {
    cleanup_parts(repository, &session.id).await?;
    release_capacity(
        repository,
        &session.id,
        &CapacityReservation {
            number: session.capacity_slot,
            generation: session.capacity_generation,
        },
    )
    .await
}

async fn cleanup_parts(repository: &Repository, id: &str) -> Result<()> {
    let prefix = repository
        .layout
        .repo_path(&format!("s3/multipart/parts/{id}/"));
    repository.store.delete_prefix(&prefix).await?;
    Ok(())
}

pub(crate) async fn list(repository: &Repository, now: u64) -> Result<Vec<Session>> {
    use futures_util::StreamExt as _;

    let prefix = repository.layout.repo_path("s3/multipart/capacity/");
    let slots = repository
        .store
        .list_prefix_bounded(&prefix, MAX_CAPACITY_SLOTS)
        .await?
        .ok_or(Error::Capacity)?;
    let results = futures_util::stream::iter(slots)
        .map(|object| list_session_for_slot(repository, object.location, now))
        .buffer_unordered(CATALOG_READ_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut sessions = Vec::new();
    for result in results {
        if let Some(session) = result? {
            sessions.push(session);
        }
    }
    sessions.sort_by(|left, right| (&left.key, &left.id).cmp(&(&right.key, &right.id)));
    Ok(sessions)
}

async fn list_session_for_slot(
    repository: &Repository,
    path: object_store::path::Path,
    now: u64,
) -> Result<Option<Session>> {
    let slot = load_capacity_path(repository, &path).await?.slot;
    let Some(owner) = slot.owner else {
        return Ok(None);
    };
    let loaded = match load(repository, &owner.upload_id).await {
        Ok(loaded) => loaded,
        Err(Error::NoSuchUpload) => return Ok(None),
        Err(error) => return Err(error),
    };
    if loaded.session.capacity_slot != slot.number
        || loaded.session.capacity_generation != slot.generation
        || loaded.session.expires_seconds != owner.expires_seconds
    {
        return Err(Error::Identity);
    }
    if !matches!(loaded.session.state, State::Open) {
        return Ok(None);
    }
    if now >= loaded.session.expires_seconds {
        return match abort(repository, loaded).await {
            Ok(()) | Err(Error::Conflict) | Err(Error::NotOpen) => Ok(None),
            Err(error) => Err(error),
        };
    }
    Ok(Some(loaded.session))
}

#[derive(Default)]
pub(crate) struct SweepStats {
    pub(crate) expired: usize,
    pub(crate) terminal_cleanups: usize,
    pub(crate) missing_cleanups: usize,
    pub(crate) published_recoveries: usize,
    completing: Vec<Loaded>,
}

impl SweepStats {
    pub(crate) fn take_completing(&mut self) -> Vec<Loaded> {
        std::mem::take(&mut self.completing)
    }
}

pub(crate) async fn sweep(repository: &Repository, now: u64) -> Result<SweepStats> {
    use futures_util::StreamExt as _;

    let prefix = repository.layout.repo_path("s3/multipart/capacity/");
    let slots = repository
        .store
        .list_prefix_bounded(&prefix, MAX_CAPACITY_SLOTS)
        .await?
        .ok_or(Error::Capacity)?;
    let reconciled = futures_util::stream::iter(slots)
        .map(|object| reconcile_capacity_slot(repository, object.location, now))
        .buffer_unordered(CATALOG_READ_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let stats = reconciled
        .into_iter()
        .fold(SweepStats::default(), |mut total, mut item| {
            total.expired += item.expired;
            total.terminal_cleanups += item.terminal_cleanups;
            total.missing_cleanups += item.missing_cleanups;
            total.completing.append(&mut item.completing);
            total
        });
    Ok(stats)
}

async fn reconcile_capacity_slot(
    repository: &Repository,
    path: object_store::path::Path,
    now: u64,
) -> SweepStats {
    let loaded_slot = match load_capacity_path(repository, &path).await {
        Ok(slot) => slot,
        Err(error) => {
            tracing::warn!(%path, %error, "multipart capacity record reconciliation failed");
            return SweepStats::default();
        }
    };
    let Some(owner) = loaded_slot.slot.owner.clone() else {
        return SweepStats::default();
    };
    let reservation = CapacityReservation {
        number: loaded_slot.slot.number,
        generation: loaded_slot.slot.generation,
    };
    match load(repository, &owner.upload_id).await {
        Ok(mut loaded) => {
            if loaded.session.capacity_slot != reservation.number
                || loaded.session.capacity_generation != reservation.generation
                || loaded.session.expires_seconds != owner.expires_seconds
            {
                tracing::warn!(upload_id = %owner.upload_id, "multipart capacity ownership mismatch");
                return SweepStats::default();
            }
            match loaded.session.state {
                State::Open if now >= loaded.session.expires_seconds => {
                    loaded.session.state = State::Aborted;
                    loaded.session.revision = loaded.session.revision.saturating_add(1);
                    match save(repository, &loaded).await {
                        Ok(()) => {
                            if let Err(error) = cleanup_terminal(repository, &loaded.session).await
                            {
                                tracing::warn!(upload_id = %owner.upload_id, %error, "expired multipart cleanup failed");
                            }
                            SweepStats {
                                expired: 1,
                                ..SweepStats::default()
                            }
                        }
                        Err(Error::Conflict) => SweepStats::default(),
                        Err(error) => {
                            tracing::warn!(upload_id = %owner.upload_id, %error, "multipart expiry transition failed");
                            SweepStats::default()
                        }
                    }
                }
                State::Completed | State::Aborted => {
                    if let Err(error) = cleanup_terminal(repository, &loaded.session).await {
                        tracing::warn!(upload_id = %owner.upload_id, %error, "terminal multipart cleanup retry failed");
                        SweepStats::default()
                    } else {
                        SweepStats {
                            terminal_cleanups: 1,
                            ..SweepStats::default()
                        }
                    }
                }
                State::Completing => SweepStats {
                    completing: vec![loaded],
                    ..SweepStats::default()
                },
                State::Open => SweepStats::default(),
            }
        }
        Err(Error::NoSuchUpload) if now >= owner.expires_seconds => {
            if let Err(error) = cleanup_parts(repository, &owner.upload_id).await {
                tracing::warn!(upload_id = %owner.upload_id, %error, "unregistered multipart cleanup failed");
                return SweepStats::default();
            }
            match release_capacity(repository, &owner.upload_id, &reservation).await {
                Ok(()) => SweepStats {
                    missing_cleanups: 1,
                    ..SweepStats::default()
                },
                Err(error) => {
                    tracing::warn!(upload_id = %owner.upload_id, %error, "unregistered multipart capacity release failed");
                    SweepStats::default()
                }
            }
        }
        Err(Error::NoSuchUpload) => SweepStats::default(),
        Err(error) => {
            tracing::warn!(upload_id = %owner.upload_id, %error, "multipart state reconciliation failed");
            SweepStats::default()
        }
    }
}

async fn save(repository: &Repository, loaded: &Loaded) -> Result<()> {
    let bytes = serde_json::to_vec(&loaded.session)?;
    repository
        .store
        .update(
            &state_path(repository, &loaded.session.id),
            Bytes::from(bytes),
            loaded.etag.clone(),
        )
        .await
        .map(drop)
        .map_err(|error| match error {
            crab_storage::StorageError::StateConflict { .. } => Error::Conflict,
            error => Error::Storage(error),
        })
}

fn state_path(repository: &Repository, id: &str) -> object_store::path::Path {
    repository
        .layout
        .repo_path(&format!("s3/multipart/uploads/{id}/state.json"))
}

fn validate_id(id: &str) -> Result<()> {
    if id.len() != 26 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(Error::NoSuchUpload);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{RepositoryAccess, RepositoryConfig, RepositoryMember};

    async fn fixture() -> Repository {
        fixture_with_limits(64, 50_000_000_000_000, 604_800).await
    }

    async fn fixture_with_limits(
        max_active_multipart_uploads: usize,
        multipart_staging_bytes_per_upload: u64,
        multipart_upload_ttl_seconds: u64,
    ) -> Repository {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "multipart-test".to_owned());
        crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        Repository::new(
            RepositoryConfig {
                name: "repo".to_owned(),
                provider: crab_storage::StorageProviderKind::Local,
                bucket: "memory".to_owned(),
                prefix: "multipart-test".to_owned(),
                default_branch: "main".to_owned(),
                members: vec![RepositoryMember {
                    principal: "user".to_owned(),
                    access: RepositoryAccess::Write,
                }],
                protected_branches: vec![],
                max_active_multipart_uploads,
                multipart_staging_bytes_per_upload,
                multipart_upload_ttl_seconds,
            },
            store,
        )
        .unwrap()
    }

    async fn spool(bytes: &[u8]) -> crate::content::Spool {
        let mut writer = crate::content::SpoolWriter::new().await.unwrap();
        writer.write(bytes, u64::MAX).await.unwrap();
        writer.finish().await.unwrap()
    }

    async fn create_session(repository: &Repository) -> Session {
        create_session_at(repository, "main/file.bin", 10)
            .await
            .unwrap()
    }

    async fn create_session_at(repository: &Repository, key: &str, now: u64) -> Result<Session> {
        create(
            repository,
            Initiation {
                bucket: "repo",
                key,
                branch: "refs/heads/main",
                path: "file.bin",
                principal: "user",
                attributes: PutAttributes::default(),
                checksum_algorithm: None,
                checksum_type: None,
                now,
            },
        )
        .await
    }

    #[tokio::test]
    async fn cancelled_state_retry_stops_without_waiting() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();

        assert!(matches!(
            wait_for_state_retry("01TESTUPLOAD00000000000000", 1, 5, &cancel).await,
            Err(Error::Cancelled)
        ));
    }

    #[tokio::test]
    async fn capacity_slot_is_reused_without_stale_release() {
        let repository = fixture_with_limits(1, 50_000_000_000_000, 100).await;
        let first = create_session(&repository).await;
        let first_reservation = CapacityReservation {
            number: first.capacity_slot,
            generation: first.capacity_generation,
        };
        let saturated = create_session_at(&repository, "main/second.bin", 11).await;
        abort(&repository, load(&repository, &first.id).await.unwrap())
            .await
            .unwrap();
        let replacement = create_session_at(&repository, "main/replacement.bin", 12)
            .await
            .unwrap();
        release_capacity(&repository, &first.id, &first_reservation)
            .await
            .unwrap();
        let still_saturated = create_session_at(&repository, "main/fourth.bin", 13).await;

        assert!(
            matches!(saturated, Err(Error::Capacity))
                && replacement.capacity_generation > first.capacity_generation
                && matches!(still_saturated, Err(Error::Capacity))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_creates_cannot_exceed_distributed_capacity() {
        let repository = Arc::new(fixture_with_limits(8, 50_000_000_000_000, 100).await);
        let barrier = Arc::new(tokio::sync::Barrier::new(64));
        let mut creates = tokio::task::JoinSet::new();
        for number in 0..64 {
            let repository = Arc::clone(&repository);
            let barrier = Arc::clone(&barrier);
            creates.spawn(async move {
                barrier.wait().await;
                create_session_at(&repository, &format!("main/{number}.bin"), 10).await
            });
        }
        let mut created = 0;
        let mut saturated = 0;
        while let Some(result) = creates.join_next().await {
            match result.unwrap() {
                Ok(_) => created += 1,
                Err(Error::Capacity) => saturated += 1,
                Err(error) => panic!("unexpected create error: {error}"),
            }
        }

        assert_eq!((created, saturated), (8, 56));
    }

    #[tokio::test]
    async fn registered_parts_cannot_exceed_the_persisted_byte_budget() {
        let repository = fixture_with_limits(4, 10, 100).await;
        let session = create_session(&repository).await;
        let first_body = Bytes::from_static(b"12345678");
        let first_spool = spool(&first_body).await;
        register_part(
            &repository,
            load(&repository, &session.id).await.unwrap(),
            1,
            &first_spool,
            crate::gateway::md5_hex(&first_body),
            crate::attributes::Checksums::default(),
            11,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let second_body = Bytes::from_static(b"four");
        let second_spool = spool(&second_body).await;
        let result = register_part(
            &repository,
            load(&repository, &session.id).await.unwrap(),
            2,
            &second_spool,
            crate::gateway::md5_hex(&second_body),
            crate::attributes::Checksums::default(),
            12,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
        let reloaded = load(&repository, &session.id).await.unwrap();
        let prefix = repository
            .layout
            .repo_path(&format!("s3/multipart/parts/{}/", session.id));

        assert!(
            matches!(result, Err(Error::Capacity))
                && reloaded.session.parts.len() == 1
                && repository.store.list_prefix(&prefix).await.unwrap().len() == 1
        );
    }

    #[tokio::test]
    async fn sweep_expires_open_upload_and_releases_its_capacity() {
        let repository = fixture_with_limits(1, 50_000_000_000_000, 10).await;
        let session = create_session(&repository).await;
        let body = Bytes::from_static(b"temporary bytes");
        let body_spool = spool(&body).await;
        register_part(
            &repository,
            load(&repository, &session.id).await.unwrap(),
            1,
            &body_spool,
            crate::gateway::md5_hex(&body),
            crate::attributes::Checksums::default(),
            11,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let before = sweep(&repository, 19).await.unwrap();
        let after = sweep(&repository, 20).await.unwrap();
        let terminal = load(&repository, &session.id).await.unwrap();
        let replacement = create_session_at(&repository, "main/replacement.bin", 20).await;
        let prefix = repository
            .layout
            .repo_path(&format!("s3/multipart/parts/{}/", session.id));

        assert!(
            before.expired == 0
                && after.expired == 1
                && matches!(terminal.session.state, State::Aborted)
                && repository
                    .store
                    .list_prefix(&prefix)
                    .await
                    .unwrap()
                    .is_empty()
                && replacement.is_ok()
        );
    }

    #[tokio::test]
    async fn configuration_change_does_not_shorten_an_existing_upload() {
        let mut repository = fixture_with_limits(1, 50_000_000_000_000, 100).await;
        let session = create_session(&repository).await;
        repository.config.multipart_upload_ttl_seconds = 1;

        let stats = sweep(&repository, 20).await.unwrap();
        let loaded = load_open(&repository, &session.id, 20).await;

        assert!(stats.expired == 0 && loaded.is_ok());
    }

    #[tokio::test]
    async fn sweep_never_expires_a_frozen_completion() {
        let repository = fixture_with_limits(1, 50_000_000_000_000, 10).await;
        let session = create_session(&repository).await;
        let body = Bytes::from_static(b"frozen bytes");
        let body_spool = spool(&body).await;
        let etag = crate::gateway::md5_hex(&body);
        register_part(
            &repository,
            load(&repository, &session.id).await.unwrap(),
            1,
            &body_spool,
            etag.clone(),
            crate::attributes::Checksums::default(),
            11,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        freeze(
            &repository,
            load(&repository, &session.id).await.unwrap(),
            &[(1, etag)],
            u64::MAX,
        )
        .await
        .unwrap();

        let mut stats = sweep(&repository, 20).await.unwrap();
        let completing = stats.take_completing();
        let frozen = load(&repository, &session.id).await.unwrap();
        let saturated = create_session_at(&repository, "main/replacement.bin", 20).await;

        assert!(
            stats.expired == 0
                && completing.len() == 1
                && matches!(frozen.session.state, State::Completing)
                && matches!(saturated, Err(Error::Capacity))
        );
    }

    #[tokio::test]
    async fn sweep_reclaims_a_slot_and_payload_without_a_session_record() {
        let repository = fixture_with_limits(1, 50_000_000_000_000, 10).await;
        let upload_id = ulid::Ulid::new().to_string();
        acquire_capacity(&repository, &upload_id, 10, 20)
            .await
            .unwrap();
        let payload = repository
            .layout
            .repo_path(&format!("s3/multipart/parts/{upload_id}/1/orphan/transfer"));
        repository
            .store
            .put_exact(&payload, Bytes::from_static(b"orphan"))
            .await
            .unwrap();

        let stats = sweep(&repository, 20).await.unwrap();
        let replacement = create_session_at(&repository, "main/replacement.bin", 20).await;

        assert!(
            stats.missing_cleanups == 1
                && matches!(
                    repository.store.head(&payload).await,
                    Err(crab_storage::StorageError::NotFound { .. })
                )
                && replacement.is_ok()
        );
    }

    #[tokio::test]
    async fn parts_and_abort_survive_fresh_catalog_reads() {
        let repository = fixture().await;
        let session = create(
            &repository,
            Initiation {
                bucket: "repo",
                key: "main/file.bin",
                branch: "refs/heads/main",
                path: "file.bin",
                principal: "user",
                attributes: PutAttributes::default(),
                checksum_algorithm: None,
                checksum_type: None,
                now: 10,
            },
        )
        .await
        .unwrap();
        let loaded = load(&repository, &session.id).await.unwrap();
        let body = Bytes::from_static(b"part bytes");
        let etag = crate::gateway::md5_hex(&body);
        let spool = spool(&body).await;
        register_part(
            &repository,
            loaded,
            1,
            &spool,
            etag.clone(),
            crate::attributes::Checksums::default(),
            11,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

        let reloaded = load(&repository, &session.id).await.unwrap();
        assert_eq!(reloaded.session.parts[&1].etag, etag);
        assert_eq!(
            part_bytes(&repository, &reloaded.session.parts[&1])
                .await
                .unwrap(),
            body
        );
        assert_eq!(list(&repository, 11).await.unwrap().len(), 1);

        abort(&repository, reloaded).await.unwrap();
        assert!(list(&repository, 12).await.unwrap().is_empty());
        let terminal = load(&repository, &session.id).await.unwrap();
        assert!(
            matches!(terminal.session.state, State::Aborted)
                && matches!(
                    load_open(&repository, &session.id, 12).await,
                    Err(Error::NotOpen)
                )
                && matches!(
                    load_completion(&repository, &session.id, 12).await,
                    Err(Error::NotOpen)
                )
        );
    }

    #[tokio::test]
    async fn replacement_reclaims_the_previous_part_payload() {
        let repository = fixture().await;
        let session = create_session(&repository).await;
        let first_body = Bytes::from_static(b"first version");
        let first_spool = spool(&first_body).await;
        let first = register_part(
            &repository,
            load(&repository, &session.id).await.unwrap(),
            1,
            &first_spool,
            crate::gateway::md5_hex(&first_body),
            crate::attributes::Checksums::default(),
            11,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let second_body = Bytes::from_static(b"second version");
        let second_spool = spool(&second_body).await;
        let second = register_part(
            &repository,
            load(&repository, &session.id).await.unwrap(),
            1,
            &second_spool,
            crate::gateway::md5_hex(&second_body),
            crate::attributes::Checksums::default(),
            12,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

        let old = repository
            .store
            .head(&repository.layout.repo_path(&first.path))
            .await;
        let new_exists = repository
            .store
            .head(&repository.layout.repo_path(&second.path))
            .await
            .is_ok();
        assert_eq!(
            (
                matches!(old, Err(crab_storage::StorageError::NotFound { .. })),
                new_exists,
            ),
            (true, true)
        );
    }

    #[tokio::test]
    async fn late_registration_reclaims_its_rejected_payload() {
        let repository = fixture().await;
        let session = create_session(&repository).await;
        let stale = load(&repository, &session.id).await.unwrap();
        abort(&repository, load(&repository, &session.id).await.unwrap())
            .await
            .unwrap();
        let body = Bytes::from_static(b"late payload");
        let body_spool = spool(&body).await;

        let error = register_part(
            &repository,
            stale,
            1,
            &body_spool,
            crate::gateway::md5_hex(&body),
            crate::attributes::Checksums::default(),
            12,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
        let prefix = repository
            .layout
            .repo_path(&format!("s3/multipart/parts/{}/", session.id));

        assert!(
            matches!(error, Error::NotOpen)
                && repository
                    .store
                    .list_prefix(&prefix)
                    .await
                    .unwrap()
                    .is_empty()
        );
    }

    #[tokio::test]
    async fn parts_stream_replays_selected_parts_in_order() {
        use futures_util::TryStreamExt as _;

        let repository = fixture().await;
        let session = create(
            &repository,
            Initiation {
                bucket: "repo",
                key: "main/file.bin",
                branch: "refs/heads/main",
                path: "file.bin",
                principal: "user",
                attributes: PutAttributes::default(),
                checksum_algorithm: None,
                checksum_type: None,
                now: 10,
            },
        )
        .await
        .unwrap();
        for (number, body) in [(1, b"first ".as_slice()), (2, b"second".as_slice())] {
            let loaded = load(&repository, &session.id).await.unwrap();
            let spool = spool(body).await;
            register_part(
                &repository,
                loaded,
                number,
                &spool,
                crate::gateway::md5_hex(body),
                crate::attributes::Checksums::default(),
                10 + number as u64,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        }
        let loaded = load(&repository, &session.id).await.unwrap();
        let parts = loaded.session.parts.into_values().collect::<Vec<_>>();
        let chunks = parts_stream(&repository, &parts)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();

        assert_eq!(chunks.concat(), b"first second");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn high_fanout_distinct_parts_merge_without_client_retries() {
        let repository = Arc::new(fixture().await);
        let session = create(
            &repository,
            Initiation {
                bucket: "repo",
                key: "main/file.bin",
                branch: "refs/heads/main",
                path: "file.bin",
                principal: "user",
                attributes: PutAttributes::default(),
                checksum_algorithm: None,
                checksum_type: None,
                now: 10,
            },
        )
        .await
        .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(64));
        let mut writes = tokio::task::JoinSet::new();
        for number in 1..=64 {
            let repository = Arc::clone(&repository);
            let upload_id = session.id.clone();
            let barrier = Arc::clone(&barrier);
            writes.spawn(async move {
                let body = Bytes::from(format!("part-{number}"));
                let spool = spool(&body).await;
                let loaded = load(&repository, &upload_id).await.unwrap();
                barrier.wait().await;
                register_part(
                    &repository,
                    loaded,
                    number,
                    &spool,
                    crate::gateway::md5_hex(&body),
                    crate::attributes::Checksums::default(),
                    10 + number as u64,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
            });
        }
        while let Some(result) = writes.join_next().await {
            result.unwrap().unwrap();
        }
        let reloaded = load(&repository, &session.id).await.unwrap();
        assert_eq!(
            reloaded.session.parts.keys().copied().collect::<Vec<_>>(),
            (1..=64).collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_replacements_retain_only_the_winning_payload() {
        let repository = Arc::new(fixture().await);
        let session = create_session(&repository).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(32));
        let mut writes = tokio::task::JoinSet::new();
        for replacement in 0..32 {
            let repository = Arc::clone(&repository);
            let upload_id = session.id.clone();
            let barrier = Arc::clone(&barrier);
            writes.spawn(async move {
                let body = Bytes::from(format!("replacement-{replacement}"));
                let body_spool = spool(&body).await;
                let loaded = load(&repository, &upload_id).await.unwrap();
                barrier.wait().await;
                register_part(
                    &repository,
                    loaded,
                    1,
                    &body_spool,
                    crate::gateway::md5_hex(&body),
                    crate::attributes::Checksums::default(),
                    10 + replacement,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
            });
        }
        while let Some(result) = writes.join_next().await {
            result.unwrap().unwrap();
        }
        let reloaded = load(&repository, &session.id).await.unwrap();
        let winner = &reloaded.session.parts[&1];
        let prefix = repository
            .layout
            .repo_path(&format!("s3/multipart/parts/{}/", session.id));
        let objects = repository.store.list_prefix(&prefix).await.unwrap();

        assert_eq!(
            objects
                .iter()
                .map(|object| object.location.clone())
                .collect::<Vec<_>>(),
            vec![repository.layout.repo_path(&winner.path)]
        );
    }

    #[tokio::test]
    async fn identical_completion_can_resume_and_return_recorded_outcome() {
        let repository = fixture().await;
        let session = create(
            &repository,
            Initiation {
                bucket: "repo",
                key: "main/file.bin",
                branch: "refs/heads/main",
                path: "file.bin",
                principal: "user",
                attributes: PutAttributes::default(),
                checksum_algorithm: None,
                checksum_type: None,
                now: 10,
            },
        )
        .await
        .unwrap();
        let body = Bytes::from_static(b"final part");
        let etag = crate::gateway::md5_hex(&body);
        let loaded = load(&repository, &session.id).await.unwrap();
        let spool = spool(&body).await;
        register_part(
            &repository,
            loaded,
            1,
            &spool,
            etag.clone(),
            crate::attributes::Checksums::default(),
            11,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let selected = vec![(1, etag)];
        let loaded = load(&repository, &session.id).await.unwrap();
        freeze(&repository, loaded, &selected, 1024).await.unwrap();
        let loaded = load(&repository, &session.id).await.unwrap();
        freeze(&repository, loaded, &selected, 1024).await.unwrap();
        let checksums = crate::attributes::Checksums::default();
        record_completion_outcome(&repository, &session.id, "result-etag", &checksums)
            .await
            .unwrap();
        record_completion_outcome(&repository, &session.id, "result-etag", &checksums)
            .await
            .unwrap();
        let loaded = load(&repository, &session.id).await.unwrap();
        complete(&repository, loaded, "result-etag".to_owned(), checksums)
            .await
            .unwrap();

        let loaded = load_completion(&repository, &session.id, u64::MAX)
            .await
            .unwrap();
        assert_eq!(
            completed_etag(&loaded.session, &selected).unwrap(),
            Some("result-etag")
        );
        assert!(completed_etag(&loaded.session, &[(2, "other".to_owned())]).is_err());
    }
}
