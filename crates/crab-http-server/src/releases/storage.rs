use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::client::{Committed, InvocationError, Observed};
use crab_cell_runtime::identity::RequestId;
use uuid::Uuid;

use super::{Release, ReleaseAsset};
use crate::{
    app::{Error, Result},
    auth::Identity,
    cells::{
        RepositoryCell,
        repository::{
            AttachReleaseAsset, AttachReleaseAssetInput, AttachReleaseAssetOutcome,
            CompleteReleasePublication, CompleteReleasePublicationInput,
            CompleteReleasePublicationOutcome, CreateRelease, CreateReleaseInput,
            CreateReleaseOutcome, DeleteRelease, DeleteReleaseAsset, DeleteReleaseAssetInput,
            DeleteReleaseAssetOutcome, DeleteReleaseInput, DeleteReleaseOutcome, GetRelease,
            ListReleases, ReleaseAssetReservation, ReleaseListInput, ReleaseRecord,
            ReserveReleaseAsset, ReserveReleaseAssetInput, ReserveReleaseAssetOutcome,
            UpdateRelease, UpdateReleaseInput, UpdateReleaseOutcome,
        },
    },
    server::{Repository, Server},
};

pub(super) struct NewRelease {
    pub submission_id: String,
    pub author: Identity,
    pub tag_name: String,
    pub target_oid: String,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
}

pub(super) struct ReleaseUpdate {
    pub version: u64,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
}

pub(super) enum ReleaseMutation {
    Ready(Release),
    PublicationPending(Release),
}

#[derive(Clone)]
pub(super) struct AssetReservation {
    pub release: u64,
    pub request_id: String,
    pub expected_version: u64,
    pub name: String,
    pub content_type: String,
    pub uploader: Identity,
}

pub(super) enum AssetReservationOutcome {
    Reserved(AssetReservation),
    Attached(Release),
}

pub(super) async fn release(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    number: u64,
) -> Result<Option<Release>> {
    let routed = route(server, repo, actor, "repository.read").await?;
    query_output(
        routed
            .client
            .query::<GetRelease>(&routed.target, None, number)
            .await,
    )
    .map(|value| value.map(from_release))
}

pub(super) async fn list(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    before: Option<u64>,
    limit: u8,
    include_drafts: bool,
    query: Option<String>,
) -> Result<(Vec<Release>, Option<u64>)> {
    let routed = route(server, repo, actor, "repository.read").await?;
    let page = query_output(
        routed
            .client
            .query::<ListReleases>(
                &routed.target,
                None,
                ReleaseListInput {
                    before,
                    limit,
                    include_drafts,
                    query,
                },
            )
            .await,
    )?;
    Ok((
        page.items.into_iter().map(from_release).collect(),
        page.next,
    ))
}

pub(super) async fn create(
    server: &Server,
    repo: &Repository,
    input: NewRelease,
) -> Result<ReleaseMutation> {
    let routed = route(server, repo, &input.author, "repository.release.create").await?;
    match command_output(
        routed
            .client
            .command::<CreateRelease>(
                &routed.target,
                mutation_identity()?,
                CreateReleaseInput {
                    submission_id: submission_id(&input.submission_id)?,
                    author: to_author(&input.author),
                    tag_name: input.tag_name,
                    target_oid: input.target_oid,
                    title: input.title,
                    body: input.body,
                    prerelease: input.prerelease,
                    draft: input.draft,
                },
            )
            .await,
    )? {
        CreateReleaseOutcome::Created(value) => Ok(ReleaseMutation::Ready(from_release(*value))),
        CreateReleaseOutcome::PublicationPending(value) => {
            Ok(ReleaseMutation::PublicationPending(from_release(*value)))
        }
        CreateReleaseOutcome::RequestConflict => Err(Error::RequestConflict),
        CreateReleaseOutcome::ReleaseConflict => Err(Error::ReleaseConflict),
    }
}

pub(super) async fn update(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    number: u64,
    input: ReleaseUpdate,
) -> Result<ReleaseMutation> {
    let routed = route(server, repo, actor, "repository.release.update").await?;
    match command_output(
        routed
            .client
            .command::<UpdateRelease>(
                &routed.target,
                mutation_identity()?,
                UpdateReleaseInput {
                    number,
                    version: input.version,
                    title: input.title,
                    body: input.body,
                    prerelease: input.prerelease,
                    draft: input.draft,
                },
            )
            .await,
    )? {
        UpdateReleaseOutcome::Updated(value) => Ok(ReleaseMutation::Ready(from_release(*value))),
        UpdateReleaseOutcome::PublicationPending(value) => {
            Ok(ReleaseMutation::PublicationPending(from_release(*value)))
        }
        UpdateReleaseOutcome::NotFound => Err(Error::ReleaseNotFound),
        UpdateReleaseOutcome::Conflict => Err(Error::Conflict),
    }
}

pub(super) async fn complete_publication(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    release: &Release,
    tag_oid: String,
) -> Result<Release> {
    let pending = release
        .publication_pending
        .as_ref()
        .ok_or(Error::ReleaseConflict)?;
    let routed = route(server, repo, actor, "repository.release.update").await?;
    match command_output(
        routed
            .client
            .command::<CompleteReleasePublication>(
                &routed.target,
                mutation_identity()?,
                CompleteReleasePublicationInput {
                    number: release.number,
                    expected_version: pending.expected_version,
                    tag_oid,
                },
            )
            .await,
    )? {
        CompleteReleasePublicationOutcome::Completed(value) => Ok(from_release(*value)),
        CompleteReleasePublicationOutcome::NotFound => Err(Error::ReleaseNotFound),
        CompleteReleasePublicationOutcome::Conflict => Err(Error::ReleaseConflict),
    }
}

pub(super) async fn delete(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    number: u64,
    version: u64,
) -> Result<()> {
    let routed = route(server, repo, actor, "repository.release.update").await?;
    match command_output(
        routed
            .client
            .command::<DeleteRelease>(
                &routed.target,
                mutation_identity()?,
                DeleteReleaseInput { number, version },
            )
            .await,
    )? {
        DeleteReleaseOutcome::Deleted => Ok(()),
        DeleteReleaseOutcome::NotFound => Err(Error::ReleaseNotFound),
        DeleteReleaseOutcome::Conflict => Err(Error::Conflict),
        DeleteReleaseOutcome::PublicationPending => Err(Error::ReleaseConflict),
    }
}

pub(super) async fn reserve_asset(
    server: &Server,
    repo: &Repository,
    reservation: AssetReservation,
) -> Result<AssetReservationOutcome> {
    let routed = route(
        server,
        repo,
        &reservation.uploader,
        "repository.release.asset",
    )
    .await?;
    match command_output(
        routed
            .client
            .command::<ReserveReleaseAsset>(
                &routed.target,
                mutation_identity()?,
                ReserveReleaseAssetInput {
                    reservation: to_reservation(&reservation)?,
                },
            )
            .await,
    )? {
        ReserveReleaseAssetOutcome::Reserved(_) => {
            Ok(AssetReservationOutcome::Reserved(reservation))
        }
        ReserveReleaseAssetOutcome::Attached(value) => {
            Ok(AssetReservationOutcome::Attached(from_release(*value)))
        }
        ReserveReleaseAssetOutcome::NotFound => Err(Error::ReleaseNotFound),
        ReserveReleaseAssetOutcome::Conflict => Err(Error::Conflict),
        ReserveReleaseAssetOutcome::NameConflict => Err(Error::ReleaseAssetConflict),
        ReserveReleaseAssetOutcome::RequestConflict => Err(Error::RequestConflict),
        ReserveReleaseAssetOutcome::AssetLimit => {
            Err(Error::Invalid("Releases support at most 96 assets"))
        }
    }
}

pub(super) async fn attach_asset(
    server: &Server,
    repo: &Repository,
    reservation: &AssetReservation,
    size: u64,
    digest: String,
) -> Result<Release> {
    let routed = route(
        server,
        repo,
        &reservation.uploader,
        "repository.release.asset",
    )
    .await?;
    match command_output(
        routed
            .client
            .command::<AttachReleaseAsset>(
                &routed.target,
                mutation_identity()?,
                AttachReleaseAssetInput {
                    reservation: to_reservation(reservation)?,
                    size,
                    digest,
                },
            )
            .await,
    )? {
        AttachReleaseAssetOutcome::Attached(value) => Ok(from_release(*value)),
        AttachReleaseAssetOutcome::NotFound => Err(Error::ReleaseNotFound),
        AttachReleaseAssetOutcome::Conflict => Err(Error::Conflict),
        AttachReleaseAssetOutcome::NameConflict => Err(Error::ReleaseAssetConflict),
        AttachReleaseAssetOutcome::RequestConflict => Err(Error::RequestConflict),
        AttachReleaseAssetOutcome::AssetLimit => {
            Err(Error::Invalid("Releases support at most 96 assets"))
        }
    }
}

pub(super) async fn delete_asset(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    release: u64,
    request_id: &str,
    version: u64,
) -> Result<Release> {
    let routed = route(server, repo, actor, "repository.release.asset").await?;
    match command_output(
        routed
            .client
            .command::<DeleteReleaseAsset>(
                &routed.target,
                mutation_identity()?,
                DeleteReleaseAssetInput {
                    release,
                    request_id: submission_id(request_id)?,
                    version,
                },
            )
            .await,
    )? {
        DeleteReleaseAssetOutcome::Deleted(value) => Ok(from_release(*value)),
        DeleteReleaseAssetOutcome::NotFound => Err(Error::ReleaseAssetNotFound),
        DeleteReleaseAssetOutcome::Conflict => Err(Error::Conflict),
    }
}

fn to_reservation(value: &AssetReservation) -> Result<ReleaseAssetReservation> {
    Ok(ReleaseAssetReservation {
        release: value.release,
        request_id: submission_id(&value.request_id)?,
        expected_version: value.expected_version,
        name: value.name.clone(),
        content_type: value.content_type.clone(),
        uploader: to_author(&value.uploader),
    })
}

fn from_release(value: ReleaseRecord) -> Release {
    Release {
        number: value.number,
        request_id: Uuid::from_bytes(value.create_submission_id).to_string(),
        author: from_author(value.author),
        tag_name: value.tag_name,
        tag_oid: value.tag_oid,
        target_oid: value.target_oid,
        title: value.title,
        body: value.body,
        prerelease: value.prerelease,
        draft: value.draft,
        publication_pending: value
            .publication_pending
            .map(|pending| super::ReleasePublication {
                expected_version: pending.expected_version,
            }),
        version: value.version,
        created_at: value.created_at_ms,
        published_at: value.published_at_ms,
        updated_at: value.updated_at_ms,
        deleted: value.deleted,
        assets: value
            .assets
            .into_iter()
            .map(|asset| ReleaseAsset {
                id: Uuid::from_bytes(asset.request_id).to_string(),
                name: asset.name,
                content_type: asset.content_type,
                size: asset.size,
                digest: asset.digest,
                uploader: from_author(asset.uploader),
                created_at: asset.created_at_ms,
            })
            .collect(),
    }
}

fn to_author(value: &Identity) -> crate::cells::repository::RepositoryAuthor {
    crate::cells::repository::RepositoryAuthor {
        issuer: value.issuer.clone(),
        subject: value.subject.clone(),
        name: value.name.clone(),
    }
}

fn from_author(value: crate::cells::repository::RepositoryAuthor) -> Identity {
    Identity {
        issuer: value.issuer,
        subject: value.subject,
        name: value.name,
    }
}

async fn route(
    server: &Server,
    repository: &Repository,
    principal: &Identity,
    action: &'static str,
) -> Result<RepositoryCell> {
    let router = server.repository_cells().ok_or(Error::CellUnavailable)?;
    router
        .route(repository.id, principal, action)
        .await
        .map_err(|error| match error {
            crate::Error::Cell(source) => Error::Cell(source),
            source => Error::Repository(source),
        })
}

fn submission_id(value: &str) -> Result<[u8; 16]> {
    Uuid::parse_str(value)
        .map(Uuid::into_bytes)
        .map_err(|_| Error::Invalid("Submission ID must be a UUID"))
}

fn mutation_identity() -> Result<MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Repository)?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms
            .checked_add(60_000)
            .ok_or(Error::CellContract("Cell request expiry overflowed"))?,
    })
}

fn command_output<T>(result: std::result::Result<Committed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(committed) => Ok(committed.output),
        Err(InvocationError::Rejected(committed)) => Ok(committed.output),
        Err(InvocationError::Pending(_)) => Err(Error::CellPending),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}

fn query_output<T>(result: std::result::Result<Observed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(observed) => Ok(observed.output),
        Err(InvocationError::Rejected(_)) => Err(Error::CellContract(
            "Cell query returned a durable rejection",
        )),
        Err(InvocationError::Pending(_)) => Err(Error::CellContract(
            "Cell query returned pending mutation evidence",
        )),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}
