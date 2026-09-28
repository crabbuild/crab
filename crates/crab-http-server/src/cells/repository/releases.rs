use serde::{Deserialize, Serialize};

use super::*;

const MAX_RELEASE_ASSETS: usize = 96;
const MAX_PAGE_WIRE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleasePublication {
    pub expected_version: u64,
    pub result_version: u64,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseAssetRecord {
    pub request_id: [u8; 16],
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub digest: String,
    pub uploader: RepositoryAuthor,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseRecord {
    pub number: u64,
    pub create_submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub tag_name: String,
    pub tag_oid: Option<String>,
    pub target_oid: String,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
    pub publication_pending: Option<ReleasePublication>,
    pub version: u64,
    pub created_at_ms: u64,
    pub published_at_ms: Option<u64>,
    pub updated_at_ms: u64,
    pub deleted: bool,
    pub assets: Vec<ReleaseAssetRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateReleaseInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub tag_name: String,
    pub target_oid: String,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreateReleaseOutcome {
    Created(Box<ReleaseRecord>),
    PublicationPending(Box<ReleaseRecord>),
    RequestConflict,
    ReleaseConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateReleaseInput {
    pub number: u64,
    pub version: u64,
    pub title: String,
    pub body: String,
    pub prerelease: bool,
    pub draft: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdateReleaseOutcome {
    Updated(Box<ReleaseRecord>),
    PublicationPending(Box<ReleaseRecord>),
    NotFound,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompleteReleasePublicationInput {
    pub number: u64,
    pub expected_version: u64,
    pub tag_oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CompleteReleasePublicationOutcome {
    Completed(Box<ReleaseRecord>),
    NotFound,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteReleaseInput {
    pub number: u64,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum DeleteReleaseOutcome {
    Deleted,
    NotFound,
    Conflict,
    PublicationPending,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseAssetReservation {
    pub release: u64,
    pub request_id: [u8; 16],
    pub expected_version: u64,
    pub name: String,
    pub content_type: String,
    pub uploader: RepositoryAuthor,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReserveReleaseAssetInput {
    pub reservation: ReleaseAssetReservation,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum ReserveReleaseAssetOutcome {
    Reserved(ReleaseAssetReservation),
    Attached(Box<ReleaseRecord>),
    NotFound,
    Conflict,
    NameConflict,
    RequestConflict,
    AssetLimit,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachReleaseAssetInput {
    pub reservation: ReleaseAssetReservation,
    pub size: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum AttachReleaseAssetOutcome {
    Attached(Box<ReleaseRecord>),
    NotFound,
    Conflict,
    NameConflict,
    RequestConflict,
    AssetLimit,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteReleaseAssetInput {
    pub release: u64,
    pub request_id: [u8; 16],
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum DeleteReleaseAssetOutcome {
    Deleted(Box<ReleaseRecord>),
    NotFound,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseListInput {
    pub before: Option<u64>,
    pub limit: u8,
    pub include_drafts: bool,
    pub query: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleasePage {
    pub items: Vec<ReleaseRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseSubmissionKey {
    pub submission_id: [u8; 16],
}

pub(crate) struct CreateRelease;

impl Command for CreateRelease {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 24;
    const CODEC_VERSION: u32 = 1;
    type Input = CreateReleaseInput;
    type Output = CreateReleaseOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_create(&input)?;
        let digest = release_digest(&input);
        let existing = context.sql(&SqlBatch { statements: vec![statement(
            "SELECT payload_digest, release_number FROM repository_release_submissions WHERE request_id = ?",
            vec![SqlValue::Blob(input.submission_id.to_vec())],
        )] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    CreateReleaseOutcome::RequestConflict,
                ));
            }
            let release = load_release(context, result_u64_from_row(row, 1)?)?.ok_or(
                cellule_runtime::Error::Command("repository release submission has no release"),
            )?;
            if release.deleted {
                return Ok(CommandResult::Rejected(
                    CreateReleaseOutcome::ReleaseConflict,
                ));
            }
            return Ok(CommandResult::Success(create_outcome(release)));
        }
        let claimed = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_release_tag_claims WHERE tag_name = ?",
                vec![SqlValue::Text(input.tag_name.clone())],
            )],
        })?;
        if !claimed[0].rows.is_empty() {
            return Ok(CommandResult::Rejected(
                CreateReleaseOutcome::ReleaseConflict,
            ));
        }
        let sequence = context.sql(&SqlBatch { statements: vec![
            statement("UPDATE repository_sequences SET last = last + 1 WHERE kind = 'release' AND last < 9007199254740991", vec![]),
            statement("SELECT last FROM repository_sequences WHERE kind = 'release'", vec![]),
        ] })?;
        if sequence[0].rows_affected != 1 {
            return Err(cellule_runtime::Error::Command(
                "repository release numbering is exhausted",
            ));
        }
        let now = timestamp(context.now_ms())?;
        let number = result_u64(&sequence, 1, 0)?;
        let pending = if input.draft {
            None
        } else {
            Some(ReleasePublication {
                expected_version: 0,
                result_version: publication_result_version(0)?,
                title: input.title.clone(),
                body: input.body.clone(),
                prerelease: input.prerelease,
            })
        };
        let release = ReleaseRecord {
            number,
            create_submission_id: input.submission_id,
            author: input.author,
            tag_name: input.tag_name,
            tag_oid: None,
            target_oid: input.target_oid,
            title: input.title,
            body: input.body,
            prerelease: input.prerelease,
            draft: input.draft,
            publication_pending: pending,
            version: 1,
            created_at_ms: now,
            published_at_ms: None,
            updated_at_ms: now,
            deleted: false,
            assets: vec![],
        };
        context.sql(&SqlBatch { statements: vec![
            statement("INSERT INTO repository_release_submissions(request_id, payload_digest, release_number) VALUES (?, ?, ?)", vec![SqlValue::Blob(release.create_submission_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), integer(release.number)?]),
            insert_release(&release)?,
            statement("INSERT INTO repository_release_tag_claims(tag_name, request_id, release_number) VALUES (?, ?, ?)", vec![SqlValue::Text(release.tag_name.clone()), SqlValue::Blob(release.create_submission_id.to_vec()), integer(release.number)?]),
        ] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(create_outcome(release)))
    }
}

fn create_outcome(release: ReleaseRecord) -> CreateReleaseOutcome {
    if release.publication_pending.is_some() {
        CreateReleaseOutcome::PublicationPending(Box::new(release))
    } else {
        CreateReleaseOutcome::Created(Box::new(release))
    }
}

pub(crate) struct UpdateRelease;

impl Command for UpdateRelease {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 25;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdateReleaseInput;
    type Output = UpdateReleaseOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.number)?;
        validate_number(input.version)?;
        validate_title(&input.title)?;
        validate_body(&input.body, false)?;
        let Some(mut release) = load_release(context, input.number)? else {
            return Ok(CommandResult::Rejected(UpdateReleaseOutcome::NotFound));
        };
        if release.deleted {
            return Ok(CommandResult::Rejected(UpdateReleaseOutcome::NotFound));
        }
        if let Some(pending) = &release.publication_pending {
            let same = pending.expected_version == input.version
                && pending.title == input.title
                && pending.body == input.body
                && pending.prerelease == input.prerelease
                && !input.draft;
            return if same {
                Ok(CommandResult::Success(
                    UpdateReleaseOutcome::PublicationPending(Box::new(release)),
                ))
            } else {
                Ok(CommandResult::Rejected(UpdateReleaseOutcome::Conflict))
            };
        }
        if release.version != input.version {
            return Ok(CommandResult::Rejected(UpdateReleaseOutcome::Conflict));
        }
        let old_version = release.version;
        let now = timestamp(context.now_ms())?;
        if release.draft && !input.draft {
            release.publication_pending = Some(ReleasePublication {
                expected_version: input.version,
                result_version: next_version(input.version)?,
                title: input.title,
                body: input.body,
                prerelease: input.prerelease,
            });
            release.updated_at_ms = now;
            update_release_row(context, &release, old_version)?;
            advance_revision(context)?;
            return Ok(CommandResult::Success(
                UpdateReleaseOutcome::PublicationPending(Box::new(release)),
            ));
        }
        release.title = input.title;
        release.body = input.body;
        release.prerelease = input.prerelease;
        if !release.draft && input.draft {
            release.published_at_ms = None;
        }
        release.draft = input.draft;
        release.version = next_version(release.version)?;
        release.updated_at_ms = now;
        update_release_row(context, &release, old_version)?;
        advance_revision(context)?;
        Ok(CommandResult::Success(UpdateReleaseOutcome::Updated(
            Box::new(release),
        )))
    }
}

pub(crate) struct CompleteReleasePublication;

impl Command for CompleteReleasePublication {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 26;
    const CODEC_VERSION: u32 = 1;
    type Input = CompleteReleasePublicationInput;
    type Output = CompleteReleasePublicationOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.number)?;
        if input.expected_version > 0 {
            validate_number(input.expected_version)?;
        }
        validate_oid(&input.tag_oid)?;
        let Some(mut release) = load_release(context, input.number)? else {
            return Ok(CommandResult::Rejected(
                CompleteReleasePublicationOutcome::NotFound,
            ));
        };
        if release.deleted {
            return Ok(CommandResult::Rejected(
                CompleteReleasePublicationOutcome::NotFound,
            ));
        }
        let Some(pending) = release.publication_pending.clone() else {
            return if release.tag_oid.as_deref() == Some(&input.tag_oid)
                && release.version == publication_result_version(input.expected_version)?
            {
                Ok(CommandResult::Success(
                    CompleteReleasePublicationOutcome::Completed(Box::new(release)),
                ))
            } else {
                Ok(CommandResult::Rejected(
                    CompleteReleasePublicationOutcome::Conflict,
                ))
            };
        };
        if pending.expected_version != input.expected_version {
            return Ok(CommandResult::Rejected(
                CompleteReleasePublicationOutcome::Conflict,
            ));
        }
        let old_version = release.version;
        let now = timestamp(context.now_ms())?;
        release.title = pending.title;
        release.body = pending.body;
        release.prerelease = pending.prerelease;
        release.draft = false;
        release.tag_oid = Some(input.tag_oid);
        release.publication_pending = None;
        release.version = pending.result_version;
        release.published_at_ms = Some(now);
        release.updated_at_ms = now;
        update_release_row(context, &release, old_version)?;
        advance_revision(context)?;
        Ok(CommandResult::Success(
            CompleteReleasePublicationOutcome::Completed(Box::new(release)),
        ))
    }
}

pub(crate) struct DeleteRelease;

impl Command for DeleteRelease {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 27;
    const CODEC_VERSION: u32 = 1;
    type Input = DeleteReleaseInput;
    type Output = DeleteReleaseOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.number)?;
        validate_number(input.version)?;
        let Some(mut release) = load_release(context, input.number)? else {
            return Ok(CommandResult::Rejected(DeleteReleaseOutcome::NotFound));
        };
        if release.deleted {
            return if input.version.checked_add(1) == Some(release.version) {
                Ok(CommandResult::Success(DeleteReleaseOutcome::Deleted))
            } else {
                Ok(CommandResult::Rejected(DeleteReleaseOutcome::NotFound))
            };
        }
        if release.publication_pending.is_some() {
            return Ok(CommandResult::Rejected(
                DeleteReleaseOutcome::PublicationPending,
            ));
        }
        if release.version != input.version {
            return Ok(CommandResult::Rejected(DeleteReleaseOutcome::Conflict));
        }
        let old_version = release.version;
        release.version = next_version(release.version)?;
        release.updated_at_ms = timestamp(context.now_ms())?;
        release.deleted = true;
        context.sql(&SqlBatch { statements: vec![
            update_release_statement(&release, old_version)?,
            statement("DELETE FROM repository_release_tag_claims WHERE release_number = ? AND request_id = ?", vec![integer(release.number)?, SqlValue::Blob(release.create_submission_id.to_vec())]),
        ] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(DeleteReleaseOutcome::Deleted))
    }
}

pub(crate) struct ReserveReleaseAsset;

impl Command for ReserveReleaseAsset {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 28;
    const CODEC_VERSION: u32 = 1;
    type Input = ReserveReleaseAssetInput;
    type Output = ReserveReleaseAssetOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        let reservation = input.reservation;
        validate_reservation(&reservation)?;
        let Some(release) = load_release(context, reservation.release)? else {
            return Ok(CommandResult::Rejected(
                ReserveReleaseAssetOutcome::NotFound,
            ));
        };
        if release.deleted {
            return Ok(CommandResult::Rejected(
                ReserveReleaseAssetOutcome::NotFound,
            ));
        }
        let digest = asset_reservation_digest(&reservation);
        let existing = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, attached FROM repository_release_asset_submissions WHERE release_number = ? AND request_id = ?", vec![integer(reservation.release)?, SqlValue::Blob(reservation.request_id.to_vec())])] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    ReserveReleaseAssetOutcome::RequestConflict,
                ));
            }
            if result_u64_from_row(row, 1)? == 1 {
                return Ok(CommandResult::Success(
                    ReserveReleaseAssetOutcome::Attached(Box::new(release)),
                ));
            }
            return Ok(CommandResult::Success(
                ReserveReleaseAssetOutcome::Reserved(reservation),
            ));
        }
        if release.version != reservation.expected_version {
            return Ok(CommandResult::Rejected(
                ReserveReleaseAssetOutcome::Conflict,
            ));
        }
        if release.assets.len() >= MAX_RELEASE_ASSETS {
            return Ok(CommandResult::Rejected(
                ReserveReleaseAssetOutcome::AssetLimit,
            ));
        }
        let name = context.sql(&SqlBatch { statements: vec![
            statement("SELECT 1 FROM repository_release_assets WHERE release_number = ? AND name = ?", vec![integer(reservation.release)?, SqlValue::Text(reservation.name.clone())]),
            statement("SELECT 1 FROM repository_release_asset_submissions WHERE release_number = ? AND name = ?", vec![integer(reservation.release)?, SqlValue::Text(reservation.name.clone())]),
        ] })?;
        if name.iter().any(|result| !result.rows.is_empty()) {
            return Ok(CommandResult::Rejected(
                ReserveReleaseAssetOutcome::NameConflict,
            ));
        }
        context.sql(&SqlBatch { statements: vec![statement("INSERT INTO repository_release_asset_submissions(release_number, request_id, payload_digest, expected_version, name) VALUES (?, ?, ?, ?, ?)", vec![integer(reservation.release)?, SqlValue::Blob(reservation.request_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), integer(reservation.expected_version)?, SqlValue::Text(reservation.name.clone())])] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(
            ReserveReleaseAssetOutcome::Reserved(reservation),
        ))
    }
}

pub(crate) struct AttachReleaseAsset;

impl Command for AttachReleaseAsset {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 29;
    const CODEC_VERSION: u32 = 1;
    type Input = AttachReleaseAssetInput;
    type Output = AttachReleaseAssetOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_reservation(&input.reservation)?;
        validate_asset_result(input.size, &input.digest)?;
        let Some(mut release) = load_release(context, input.reservation.release)? else {
            return Ok(CommandResult::Rejected(AttachReleaseAssetOutcome::NotFound));
        };
        if release.deleted {
            return Ok(CommandResult::Rejected(AttachReleaseAssetOutcome::NotFound));
        }
        let digest = asset_reservation_digest(&input.reservation);
        let submission = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, attached FROM repository_release_asset_submissions WHERE release_number = ? AND request_id = ?", vec![integer(input.reservation.release)?, SqlValue::Blob(input.reservation.request_id.to_vec())])] })?;
        let Some(row) = submission[0].rows.first() else {
            return Ok(CommandResult::Rejected(
                AttachReleaseAssetOutcome::RequestConflict,
            ));
        };
        if result_blob(row, 0)? != digest.as_bytes() {
            return Ok(CommandResult::Rejected(
                AttachReleaseAssetOutcome::RequestConflict,
            ));
        }
        if result_u64_from_row(row, 1)? == 1 {
            let asset = release
                .assets
                .iter()
                .find(|asset| asset.request_id == input.reservation.request_id);
            return if asset.is_some_and(|asset| {
                asset.name == input.reservation.name
                    && asset.content_type == input.reservation.content_type
                    && asset.size == input.size
                    && asset.digest == input.digest
            }) {
                Ok(CommandResult::Success(AttachReleaseAssetOutcome::Attached(
                    Box::new(release),
                )))
            } else {
                Ok(CommandResult::Rejected(
                    AttachReleaseAssetOutcome::RequestConflict,
                ))
            };
        }
        if release.version != input.reservation.expected_version {
            return Ok(CommandResult::Rejected(AttachReleaseAssetOutcome::Conflict));
        }
        if release.assets.len() >= MAX_RELEASE_ASSETS {
            return Ok(CommandResult::Rejected(
                AttachReleaseAssetOutcome::AssetLimit,
            ));
        }
        if release
            .assets
            .iter()
            .any(|asset| asset.name == input.reservation.name)
        {
            return Ok(CommandResult::Rejected(
                AttachReleaseAssetOutcome::NameConflict,
            ));
        }
        let now = timestamp(context.now_ms())?;
        let asset = ReleaseAssetRecord {
            request_id: input.reservation.request_id,
            name: input.reservation.name,
            content_type: input.reservation.content_type,
            size: input.size,
            digest: input.digest,
            uploader: input.reservation.uploader,
            created_at_ms: now,
        };
        let old_version = release.version;
        release.version = next_version(release.version)?;
        release.updated_at_ms = now;
        context.sql(&SqlBatch { statements: vec![
            statement("INSERT INTO repository_release_assets(release_number, request_id, name, content_type, size, digest, uploader_issuer, uploader_subject, uploader_name, created_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", vec![integer(release.number)?, SqlValue::Blob(asset.request_id.to_vec()), SqlValue::Text(asset.name.clone()), SqlValue::Text(asset.content_type.clone()), integer(asset.size)?, SqlValue::Text(asset.digest.clone()), SqlValue::Text(asset.uploader.issuer.clone()), SqlValue::Text(asset.uploader.subject.clone()), SqlValue::Text(asset.uploader.name.clone()), integer(asset.created_at_ms)?]),
            statement("UPDATE repository_release_asset_submissions SET attached = 1 WHERE release_number = ? AND request_id = ? AND attached = 0", vec![integer(release.number)?, SqlValue::Blob(asset.request_id.to_vec())]),
            update_release_statement(&release, old_version)?,
        ] })?;
        release.assets.push(asset);
        release
            .assets
            .sort_by(|left, right| left.name.cmp(&right.name));
        advance_revision(context)?;
        Ok(CommandResult::Success(AttachReleaseAssetOutcome::Attached(
            Box::new(release),
        )))
    }
}

pub(crate) struct DeleteReleaseAsset;

impl Command for DeleteReleaseAsset {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 30;
    const CODEC_VERSION: u32 = 1;
    type Input = DeleteReleaseAssetInput;
    type Output = DeleteReleaseAssetOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.release)?;
        validate_number(input.version)?;
        let Some(mut release) = load_release(context, input.release)? else {
            return Ok(CommandResult::Rejected(DeleteReleaseAssetOutcome::NotFound));
        };
        if release.deleted {
            return Ok(CommandResult::Rejected(DeleteReleaseAssetOutcome::NotFound));
        }
        if release.version != input.version {
            return Ok(CommandResult::Rejected(DeleteReleaseAssetOutcome::Conflict));
        }
        let Some(index) = release
            .assets
            .iter()
            .position(|asset| asset.request_id == input.request_id)
        else {
            return Ok(CommandResult::Rejected(DeleteReleaseAssetOutcome::NotFound));
        };
        let old_version = release.version;
        release.assets.remove(index);
        release.version = next_version(release.version)?;
        release.updated_at_ms = timestamp(context.now_ms())?;
        context.sql(&SqlBatch { statements: vec![
            statement("DELETE FROM repository_release_assets WHERE release_number = ? AND request_id = ?", vec![integer(release.number)?, SqlValue::Blob(input.request_id.to_vec())]),
            statement("DELETE FROM repository_release_asset_submissions WHERE release_number = ? AND request_id = ?", vec![integer(release.number)?, SqlValue::Blob(input.request_id.to_vec())]),
            update_release_statement(&release, old_version)?,
        ] })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(DeleteReleaseAssetOutcome::Deleted(
            Box::new(release),
        )))
    }
}

pub(crate) struct GetRelease;

impl Query for GetRelease {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 24;
    const CODEC_VERSION: u32 = 1;
    type Input = u64;
    type Output = Option<ReleaseRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        number: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        validate_number(number)?;
        load_release_query(context, number)
    }
}

pub(crate) struct GetReleaseSubmission;

impl Query for GetReleaseSubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 25;
    const CODEC_VERSION: u32 = 1;
    type Input = ReleaseSubmissionKey;
    type Output = Option<ReleaseRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT release_number FROM repository_release_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(key.submission_id.to_vec())],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .map(|number| load_release_query(context, number))
            .transpose()
            .map(Option::flatten)
    }
}

pub(crate) struct ListReleases;

impl Query for ListReleases {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 26;
    const CODEC_VERSION: u32 = 1;
    type Input = ReleaseListInput;
    type Output = ReleasePage;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> cellule_runtime::Result<Self::Output> {
        validate_list(input.before, input.limit)?;
        validate_query(input.query.as_deref())?;
        let query = input.query.as_ref().map(|query| query.to_lowercase());
        let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
        let result = context.sql(&SqlBatch { statements: vec![statement("SELECT number FROM repository_releases WHERE number <= ? ORDER BY number DESC LIMIT 200", vec![integer(upper)?])] })?;
        let mut items = Vec::new();
        let mut last_examined = None;
        for row in &result[0].rows {
            let number = result_u64_from_row(row, 0)?;
            last_examined = Some(number);
            let release = load_release_query(context, number)?.ok_or(
                cellule_runtime::Error::Command("repository release list row disappeared"),
            )?;
            let visible = !release.deleted
                && (input.include_drafts
                    || (!release.draft && release.publication_pending.is_none()));
            let matches = query.as_ref().is_none_or(|query| {
                [
                    &release.tag_name,
                    &release.title,
                    &release.body,
                    &release.author.name,
                ]
                .iter()
                .any(|value| value.to_lowercase().contains(query))
            });
            if visible && matches {
                items.push(release);
            }
            if items.len() == usize::from(input.limit) {
                break;
            }
        }
        let scanned_to_limit = result[0].rows.len() == MAX_LIST_SCAN as usize
            || items.len() == usize::from(input.limit);
        let (items, size_limited) = bounded_release_page(items)?;
        let next = if size_limited {
            items.last().map(|release| release.number)
        } else if scanned_to_limit {
            last_examined
        } else {
            None
        };
        Ok(ReleasePage { items, next })
    }
}

fn validate_create(input: &CreateReleaseInput) -> cellule_runtime::Result<()> {
    validate_author(&input.author)?;
    validate_tag_name(&input.tag_name)?;
    validate_oid(&input.target_oid)?;
    validate_title(&input.title)?;
    validate_body(&input.body, false)
}

fn validate_tag_name(value: &str) -> cellule_runtime::Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value.trim() != value
        || value.starts_with("refs/")
        || value.chars().any(char::is_control)
        || crab_git::validate_push_refname(&format!("refs/tags/{value}")).is_err()
    {
        return Err(cellule_runtime::Error::Command(
            "repository release tag is invalid",
        ));
    }
    Ok(())
}

fn validate_oid(value: &str) -> cellule_runtime::Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(cellule_runtime::Error::Command(
            "repository release object ID is invalid",
        ));
    }
    Ok(())
}

fn validate_reservation(value: &ReleaseAssetReservation) -> cellule_runtime::Result<()> {
    validate_number(value.release)?;
    validate_number(value.expected_version)?;
    validate_author(&value.uploader)?;
    if value.name.is_empty()
        || value.name.len() > 255
        || !value.name.is_ascii()
        || value.name.starts_with('.')
        || value.name.ends_with('.')
        || value
            .name
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'/' | b'\\' | b'"'))
        || value.content_type.is_empty()
        || value.content_type.len() > 255
        || value.content_type.chars().any(char::is_control)
    {
        return Err(cellule_runtime::Error::Command(
            "repository release asset reservation is invalid",
        ));
    }
    Ok(())
}

fn validate_asset_result(size: u64, digest: &str) -> cellule_runtime::Result<()> {
    if size > crate::server::MAX_DEPENDENCY_FILE_BYTES
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(cellule_runtime::Error::Command(
            "repository release asset result is invalid",
        ));
    }
    Ok(())
}

fn release_digest(input: &CreateReleaseInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.release-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    for value in [
        &input.tag_name,
        &input.target_oid,
        &input.title,
        &input.body,
    ] {
        hash_text(&mut hasher, value);
    }
    hasher.update(&[u8::from(input.prerelease), u8::from(input.draft)]);
    hasher.finalize()
}

fn asset_reservation_digest(value: &ReleaseAssetReservation) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.release-asset-reservation.v1\0");
    hasher.update(&value.release.to_be_bytes());
    hasher.update(&value.expected_version.to_be_bytes());
    hash_text(&mut hasher, &value.name);
    hash_text(&mut hasher, &value.content_type);
    hash_text(&mut hasher, &value.uploader.issuer);
    hash_text(&mut hasher, &value.uploader.subject);
    hasher.finalize()
}

fn next_version(value: u64) -> cellule_runtime::Result<u64> {
    value
        .checked_add(1)
        .filter(|value| *value < MAX_NUMBER)
        .ok_or(cellule_runtime::Error::Command(
            "repository release version is exhausted",
        ))
}

fn publication_result_version(expected_version: u64) -> cellule_runtime::Result<u64> {
    if expected_version == 0 {
        Ok(1)
    } else {
        next_version(expected_version)
    }
}

fn optional_wire<T: WireValue>(value: &Option<T>) -> cellule_runtime::Result<SqlValue> {
    match value {
        Some(value) => {
            let mut encoder = BoundedEncoder::new(512 * 1024).map_err(|_| {
                cellule_runtime::Error::Command("repository release publication is too large")
            })?;
            value.encode(&mut encoder).map_err(|_| {
                cellule_runtime::Error::Command("repository release publication is too large")
            })?;
            Ok(SqlValue::Blob(encoder.finish()))
        }
        None => Ok(SqlValue::Null),
    }
}

fn decode_optional_wire<T: WireValue>(value: &SqlValue) -> cellule_runtime::Result<Option<T>> {
    match value {
        SqlValue::Null => Ok(None),
        SqlValue::Blob(bytes) => {
            let mut decoder = BoundedDecoder::new(bytes, 512 * 1024).map_err(|_| {
                cellule_runtime::Error::Command("repository release publication is invalid")
            })?;
            let value = T::decode(&mut decoder).map_err(|_| {
                cellule_runtime::Error::Command("repository release publication is invalid")
            })?;
            decoder.finish().map_err(|_| {
                cellule_runtime::Error::Command("repository release publication is invalid")
            })?;
            Ok(Some(value))
        }
        _ => Err(cellule_runtime::Error::Command(
            "repository release publication is invalid",
        )),
    }
}

fn insert_release(release: &ReleaseRecord) -> cellule_runtime::Result<SqlStatement> {
    Ok(statement(
        "INSERT INTO repository_releases(number, create_request_id, author_issuer, author_subject, author_name, tag_name, tag_oid, target_oid, title, body, prerelease, draft, publication_pending, version, created_at_ms, published_at_ms, updated_at_ms, deleted) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        release_values(release)?,
    ))
}

fn release_values(release: &ReleaseRecord) -> cellule_runtime::Result<Vec<SqlValue>> {
    Ok(vec![
        integer(release.number)?,
        SqlValue::Blob(release.create_submission_id.to_vec()),
        SqlValue::Text(release.author.issuer.clone()),
        SqlValue::Text(release.author.subject.clone()),
        SqlValue::Text(release.author.name.clone()),
        SqlValue::Text(release.tag_name.clone()),
        release
            .tag_oid
            .clone()
            .map_or(SqlValue::Null, SqlValue::Text),
        SqlValue::Text(release.target_oid.clone()),
        SqlValue::Text(release.title.clone()),
        SqlValue::Text(release.body.clone()),
        SqlValue::Integer(i64::from(release.prerelease)),
        SqlValue::Integer(i64::from(release.draft)),
        optional_wire(&release.publication_pending)?,
        integer(release.version)?,
        integer(release.created_at_ms)?,
        release
            .published_at_ms
            .map(integer)
            .transpose()?
            .unwrap_or(SqlValue::Null),
        integer(release.updated_at_ms)?,
        SqlValue::Integer(i64::from(release.deleted)),
    ])
}

fn update_release_statement(
    release: &ReleaseRecord,
    expected_version: u64,
) -> cellule_runtime::Result<SqlStatement> {
    Ok(statement(
        "UPDATE repository_releases SET tag_oid = ?, title = ?, body = ?, prerelease = ?, draft = ?, publication_pending = ?, version = ?, published_at_ms = ?, updated_at_ms = ?, deleted = ? WHERE number = ? AND version = ?",
        vec![
            release
                .tag_oid
                .clone()
                .map_or(SqlValue::Null, SqlValue::Text),
            SqlValue::Text(release.title.clone()),
            SqlValue::Text(release.body.clone()),
            SqlValue::Integer(i64::from(release.prerelease)),
            SqlValue::Integer(i64::from(release.draft)),
            optional_wire(&release.publication_pending)?,
            integer(release.version)?,
            release
                .published_at_ms
                .map(integer)
                .transpose()?
                .unwrap_or(SqlValue::Null),
            integer(release.updated_at_ms)?,
            SqlValue::Integer(i64::from(release.deleted)),
            integer(release.number)?,
            integer(expected_version)?,
        ],
    ))
}

fn update_release_row(
    context: &CommandContext<'_, '_>,
    release: &ReleaseRecord,
    expected_version: u64,
) -> cellule_runtime::Result<()> {
    let result = context.sql(&SqlBatch {
        statements: vec![update_release_statement(release, expected_version)?],
    })?;
    if result[0].rows_affected != 1 {
        return Err(cellule_runtime::Error::Command(
            "repository release update lost its transaction",
        ));
    }
    Ok(())
}

fn release_select() -> &'static str {
    "SELECT number, create_request_id, author_issuer, author_subject, author_name, tag_name, tag_oid, target_oid, title, body, prerelease, draft, publication_pending, version, created_at_ms, published_at_ms, updated_at_ms, deleted FROM repository_releases WHERE number = ?"
}

fn load_release(
    context: &CommandContext<'_, '_>,
    number: u64,
) -> cellule_runtime::Result<Option<ReleaseRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![
        statement(release_select(), vec![integer(number)?]),
        statement("SELECT request_id, name, content_type, size, digest, uploader_issuer, uploader_subject, uploader_name, created_at_ms FROM repository_release_assets WHERE release_number = ? ORDER BY name", vec![integer(number)?]),
    ] })?;
    result[0]
        .rows
        .first()
        .map(|row| release_from_rows(row, &result[1].rows))
        .transpose()
}

fn load_release_query(
    context: &QueryContext<'_>,
    number: u64,
) -> cellule_runtime::Result<Option<ReleaseRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![
        statement(release_select(), vec![integer(number)?]),
        statement("SELECT request_id, name, content_type, size, digest, uploader_issuer, uploader_subject, uploader_name, created_at_ms FROM repository_release_assets WHERE release_number = ? ORDER BY name", vec![integer(number)?]),
    ] })?;
    result[0]
        .rows
        .first()
        .map(|row| release_from_rows(row, &result[1].rows))
        .transpose()
}

fn release_from_rows(
    row: &[SqlValue],
    asset_rows: &[Vec<SqlValue>],
) -> cellule_runtime::Result<ReleaseRecord> {
    let submission = <[u8; 16]>::try_from(result_blob(row, 1)?).map_err(|_| {
        cellule_runtime::Error::Command("repository release submission ID is invalid")
    })?;
    let assets = asset_rows
        .iter()
        .map(|row| asset_from_row(row))
        .collect::<cellule_runtime::Result<Vec<_>>>()?;
    Ok(ReleaseRecord {
        number: result_u64_from_row(row, 0)?,
        create_submission_id: submission,
        author: author_from_row(row, 2)?,
        tag_name: result_text(row, 5)?,
        tag_oid: optional_text(row, 6)?,
        target_oid: result_text(row, 7)?,
        title: result_text(row, 8)?,
        body: result_text(row, 9)?,
        prerelease: result_bool(row, 10)?,
        draft: result_bool(row, 11)?,
        publication_pending: decode_optional_wire(&row[12])?,
        version: result_u64_from_row(row, 13)?,
        created_at_ms: result_u64_from_row(row, 14)?,
        published_at_ms: optional_u64(row, 15)?,
        updated_at_ms: result_u64_from_row(row, 16)?,
        deleted: result_bool(row, 17)?,
        assets,
    })
}

fn asset_from_row(row: &[SqlValue]) -> cellule_runtime::Result<ReleaseAssetRecord> {
    Ok(ReleaseAssetRecord {
        request_id: <[u8; 16]>::try_from(result_blob(row, 0)?).map_err(|_| {
            cellule_runtime::Error::Command("repository release asset ID is invalid")
        })?,
        name: result_text(row, 1)?,
        content_type: result_text(row, 2)?,
        size: result_u64_from_row(row, 3)?,
        digest: result_text(row, 4)?,
        uploader: author_from_row(row, 5)?,
        created_at_ms: result_u64_from_row(row, 8)?,
    })
}

fn author_from_row(row: &[SqlValue], start: usize) -> cellule_runtime::Result<RepositoryAuthor> {
    Ok(RepositoryAuthor {
        issuer: result_text(row, start)?,
        subject: result_text(row, start + 1)?,
        name: result_text(row, start + 2)?,
    })
}

fn optional_text(row: &[SqlValue], index: usize) -> cellule_runtime::Result<Option<String>> {
    match row.get(index) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        _ => Err(cellule_runtime::Error::Command(
            "repository release row is invalid",
        )),
    }
}

fn optional_u64(row: &[SqlValue], index: usize) -> cellule_runtime::Result<Option<u64>> {
    match row.get(index) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| cellule_runtime::Error::Command("repository release row is invalid")),
        _ => Err(cellule_runtime::Error::Command(
            "repository release row is invalid",
        )),
    }
}

fn result_bool(row: &[SqlValue], index: usize) -> cellule_runtime::Result<bool> {
    match result_u64_from_row(row, index)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(cellule_runtime::Error::Command(
            "repository release row is invalid",
        )),
    }
}

fn bounded_release_page(
    items: Vec<ReleaseRecord>,
) -> cellule_runtime::Result<(Vec<ReleaseRecord>, bool)> {
    let total = items.len();
    let mut used =
        4 + br#"{"items":["#.len() + br#"],"next":"#.len() + MAX_NUMBER.to_string().len() + 1;
    let mut kept = Vec::with_capacity(total);
    for item in items {
        let encoded = serde_json::to_vec(&item)
            .map_err(|_| cellule_runtime::Error::Command("repository release row is invalid"))?;
        let separator = usize::from(!kept.is_empty());
        if used
            .checked_add(separator + encoded.len())
            .is_none_or(|size| size > MAX_PAGE_WIRE_BYTES)
        {
            break;
        }
        used += separator + encoded.len();
        kept.push(item);
    }
    if kept.is_empty() && total != 0 {
        return Err(cellule_runtime::Error::Command(
            "repository release exceeds the page limit",
        ));
    }
    let limited = kept.len() < total;
    Ok((kept, limited))
}
