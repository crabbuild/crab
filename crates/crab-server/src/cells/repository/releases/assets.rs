use super::*;

pub(crate) struct ReserveReleaseAsset;

impl Mutation for ReserveReleaseAsset {
    const ID: u32 = 28;
    type Input = ReserveReleaseAssetInput;
    type Output = ReserveReleaseAssetOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        let reservation = input.reservation;
        validate_reservation(&reservation)?;
        let Some(release) = load_release(context, reservation.release)? else {
            return Ok(MutationResult::Rejected(
                ReserveReleaseAssetOutcome::NotFound,
            ));
        };
        if release.deleted {
            return Ok(MutationResult::Rejected(
                ReserveReleaseAssetOutcome::NotFound,
            ));
        }
        let digest = asset_reservation_digest(&reservation);
        let existing = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, attached FROM repository_release_asset_submissions WHERE release_number = ? AND request_id = ?", vec![integer(reservation.release)?, SqlValue::Blob(reservation.request_id.to_vec())])] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    ReserveReleaseAssetOutcome::RequestConflict,
                ));
            }
            if result_u64_from_row(row, 1)? == 1 {
                return Ok(MutationResult::Success(
                    ReserveReleaseAssetOutcome::Attached(Box::new(release)),
                ));
            }
            return Ok(MutationResult::Success(
                ReserveReleaseAssetOutcome::Reserved(reservation),
            ));
        }
        if release.version != reservation.expected_version {
            return Ok(MutationResult::Rejected(
                ReserveReleaseAssetOutcome::Conflict,
            ));
        }
        if release.assets.len() >= MAX_RELEASE_ASSETS {
            return Ok(MutationResult::Rejected(
                ReserveReleaseAssetOutcome::AssetLimit,
            ));
        }
        let name = context.sql(&SqlBatch { statements: vec![
            statement("SELECT 1 FROM repository_release_assets WHERE release_number = ? AND name = ?", vec![integer(reservation.release)?, SqlValue::Text(reservation.name.clone())]),
            statement("SELECT 1 FROM repository_release_asset_submissions WHERE release_number = ? AND name = ?", vec![integer(reservation.release)?, SqlValue::Text(reservation.name.clone())]),
        ] })?;
        if name.iter().any(|result| !result.rows.is_empty()) {
            return Ok(MutationResult::Rejected(
                ReserveReleaseAssetOutcome::NameConflict,
            ));
        }
        context.sql(&SqlBatch { statements: vec![statement("INSERT INTO repository_release_asset_submissions(release_number, request_id, payload_digest, expected_version, name) VALUES (?, ?, ?, ?, ?)", vec![integer(reservation.release)?, SqlValue::Blob(reservation.request_id.to_vec()), SqlValue::Blob(digest.as_bytes().to_vec()), integer(reservation.expected_version)?, SqlValue::Text(reservation.name.clone())])] })?;
        advance_revision(context)?;
        Ok(MutationResult::Success(
            ReserveReleaseAssetOutcome::Reserved(reservation),
        ))
    }
}

pub(crate) struct AttachReleaseAsset;

impl Mutation for AttachReleaseAsset {
    const ID: u32 = 29;
    type Input = AttachReleaseAssetInput;
    type Output = AttachReleaseAssetOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_reservation(&input.reservation)?;
        validate_asset_result(input.size, &input.digest)?;
        let Some(mut release) = load_release(context, input.reservation.release)? else {
            return Ok(MutationResult::Rejected(
                AttachReleaseAssetOutcome::NotFound,
            ));
        };
        if release.deleted {
            return Ok(MutationResult::Rejected(
                AttachReleaseAssetOutcome::NotFound,
            ));
        }
        let digest = asset_reservation_digest(&input.reservation);
        let submission = context.sql(&SqlBatch { statements: vec![statement("SELECT payload_digest, attached FROM repository_release_asset_submissions WHERE release_number = ? AND request_id = ?", vec![integer(input.reservation.release)?, SqlValue::Blob(input.reservation.request_id.to_vec())])] })?;
        let Some(row) = submission[0].rows.first() else {
            return Ok(MutationResult::Rejected(
                AttachReleaseAssetOutcome::RequestConflict,
            ));
        };
        if result_blob(row, 0)? != digest.as_bytes() {
            return Ok(MutationResult::Rejected(
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
                Ok(MutationResult::Success(
                    AttachReleaseAssetOutcome::Attached(Box::new(release)),
                ))
            } else {
                Ok(MutationResult::Rejected(
                    AttachReleaseAssetOutcome::RequestConflict,
                ))
            };
        }
        if release.version != input.reservation.expected_version {
            return Ok(MutationResult::Rejected(
                AttachReleaseAssetOutcome::Conflict,
            ));
        }
        if release.assets.len() >= MAX_RELEASE_ASSETS {
            return Ok(MutationResult::Rejected(
                AttachReleaseAssetOutcome::AssetLimit,
            ));
        }
        if release
            .assets
            .iter()
            .any(|asset| asset.name == input.reservation.name)
        {
            return Ok(MutationResult::Rejected(
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
        Ok(MutationResult::Success(
            AttachReleaseAssetOutcome::Attached(Box::new(release)),
        ))
    }
}

pub(crate) struct DeleteReleaseAsset;

impl Mutation for DeleteReleaseAsset {
    const ID: u32 = 30;
    type Input = DeleteReleaseAssetInput;
    type Output = DeleteReleaseAssetOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_number(input.release)?;
        validate_number(input.version)?;
        let Some(mut release) = load_release(context, input.release)? else {
            return Ok(MutationResult::Rejected(
                DeleteReleaseAssetOutcome::NotFound,
            ));
        };
        if release.deleted {
            return Ok(MutationResult::Rejected(
                DeleteReleaseAssetOutcome::NotFound,
            ));
        }
        if release.version != input.version {
            return Ok(MutationResult::Rejected(
                DeleteReleaseAssetOutcome::Conflict,
            ));
        }
        let Some(index) = release
            .assets
            .iter()
            .position(|asset| asset.request_id == input.request_id)
        else {
            return Ok(MutationResult::Rejected(
                DeleteReleaseAssetOutcome::NotFound,
            ));
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
        Ok(MutationResult::Success(DeleteReleaseAssetOutcome::Deleted(
            Box::new(release),
        )))
    }
}
