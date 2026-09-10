use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use crab_storage::ETag;
use serde::{Deserialize, Serialize};

use crate::{attributes::PutAttributes, gateway::Repository};

const VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PARTS: usize = 10_000;
// One admitted high-fanout part burst should converge in the gateway. Conflicts
// beyond this bound remain retryable across independently scaled instances.
const MAX_STATE_UPDATE_ATTEMPTS: usize = 64;
const MAX_STATE_RETRY_EXPONENT: usize = 5;

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
    #[error("multipart state changed concurrently")]
    Conflict,
    #[error("multipart state update was cancelled")]
    Cancelled,
    #[error("multipart record is corrupt")]
    Decode(#[from] serde_json::Error),
    #[error("multipart storage failed")]
    Storage(#[from] crab_storage::StorageError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

pub(crate) async fn create(repository: &Repository, initiation: Initiation<'_>) -> Result<Session> {
    for _ in 0..8 {
        let id = ulid::Ulid::new().to_string();
        let session = Session {
            version: VERSION,
            id: id.clone(),
            bucket: initiation.bucket.to_owned(),
            key: initiation.key.to_owned(),
            branch: initiation.branch.to_owned(),
            path: initiation.path.to_owned(),
            principal: initiation.principal.to_owned(),
            created_seconds: initiation.now,
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
            Err(crab_storage::StorageError::StateConflict { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(Error::Conflict)
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

pub(crate) fn authorize(session: &Session, bucket: &str, key: &str, principal: &str) -> Result<()> {
    if session.bucket != bucket || session.key != key || session.principal != principal {
        return Err(Error::Identity);
    }
    Ok(())
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
    let path = format!("s3/multipart/parts/{}/{number}/{etag}", loaded.session.id);
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
            return Err(Error::NotOpen);
        }
        loaded.session.parts.insert(number, part.clone());
        if loaded.session.parts.len() > MAX_PARTS {
            return Err(Error::PartNumber);
        }
        loaded.session.revision = loaded.session.revision.saturating_add(1);
        match save(repository, &loaded).await {
            Ok(()) => return Ok(part),
            Err(Error::Conflict) if attempt + 1 < MAX_STATE_UPDATE_ATTEMPTS => {
                wait_for_state_retry(&loaded.session.id, number, attempt, cancel).await?;
                loaded = load(repository, &loaded.session.id).await?;
            }
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
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
    if matches!(loaded.session.state, State::Completed)
        && loaded.session.completion_etag.as_deref() == Some(&etag)
    {
        return Ok(());
    }
    if !matches!(loaded.session.state, State::Completing) {
        return Err(Error::NotOpen);
    }
    loaded.session.state = State::Completed;
    loaded.session.completion_etag = Some(etag);
    loaded.session.completion_checksums = Some(checksums);
    loaded.session.revision = loaded.session.revision.saturating_add(1);
    save(repository, &loaded).await?;
    if let Err(error) = cleanup_parts(repository, &loaded.session.id).await {
        tracing::warn!(upload_id = %loaded.session.id, %error, "completed multipart part cleanup failed");
    }
    Ok(())
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

pub(crate) async fn abort(repository: &Repository, mut loaded: Loaded) -> Result<()> {
    if !matches!(loaded.session.state, State::Open) {
        return Err(Error::NotOpen);
    }
    loaded.session.state = State::Aborted;
    loaded.session.revision = loaded.session.revision.saturating_add(1);
    save(repository, &loaded).await?;
    cleanup_parts(repository, &loaded.session.id).await?;
    Ok(())
}

async fn cleanup_parts(repository: &Repository, id: &str) -> Result<()> {
    let prefix = repository
        .layout
        .repo_path(&format!("s3/multipart/parts/{id}/"));
    repository.store.delete_prefix(&prefix).await?;
    Ok(())
}

pub(crate) async fn list(repository: &Repository) -> Result<Vec<Session>> {
    let prefix = repository.layout.repo_path("s3/multipart/uploads/");
    let mut sessions = Vec::new();
    for object in repository.store.list_prefix(&prefix).await? {
        if !object.location.as_ref().ends_with("/state.json") {
            continue;
        }
        let (bytes, _) = repository
            .store
            .get_with_etag_bounded(&object.location, MAX_RECORD_BYTES)
            .await?;
        let session: Session = serde_json::from_slice(&bytes)?;
        if session.version == VERSION && matches!(session.state, State::Open) {
            sessions.push(session);
        }
    }
    sessions.sort_by(|left, right| (&left.key, &left.id).cmp(&(&right.key, &right.id)));
    Ok(sessions)
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
        assert_eq!(list(&repository).await.unwrap().len(), 1);

        abort(&repository, reloaded).await.unwrap();
        assert!(list(&repository).await.unwrap().is_empty());
        let terminal = load(&repository, &session.id).await.unwrap();
        assert!(matches!(terminal.session.state, State::Aborted));
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
        let loaded = load(&repository, &session.id).await.unwrap();
        complete(
            &repository,
            loaded,
            "result-etag".to_owned(),
            crate::attributes::Checksums::default(),
        )
        .await
        .unwrap();

        let loaded = load(&repository, &session.id).await.unwrap();
        assert_eq!(
            completed_etag(&loaded.session, &selected).unwrap(),
            Some("result-etag")
        );
        assert!(completed_etag(&loaded.session, &[(2, "other".to_owned())]).is_err());
    }
}
