use super::*;

pub(crate) struct CreateRelease;

impl Mutation for CreateRelease {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 24;
    type Input = CreateReleaseInput;
    type Output = CreateReleaseOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_create(&input)?;
        let digest = release_digest(&input);
        let existing = context.sql(&SqlBatch { statements: vec![statement(
            "SELECT payload_digest, release_number FROM repository_release_submissions WHERE request_id = ?",
            vec![SqlValue::Blob(input.submission_id.to_vec())],
        )] })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    CreateReleaseOutcome::RequestConflict,
                ));
            }
            let release = load_release(context, result_u64_from_row(row, 1)?)?.ok_or(
                crate::cells::Error::Invalid("repository release submission has no release"),
            )?;
            if release.deleted {
                return Ok(MutationResult::Rejected(
                    CreateReleaseOutcome::ReleaseConflict,
                ));
            }
            return Ok(MutationResult::Success(create_outcome(release)));
        }
        let claimed = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_release_tag_claims WHERE tag_name = ?",
                vec![SqlValue::Text(input.tag_name.clone())],
            )],
        })?;
        if !claimed[0].rows.is_empty() {
            return Ok(MutationResult::Rejected(
                CreateReleaseOutcome::ReleaseConflict,
            ));
        }
        let sequence = context.sql(&SqlBatch { statements: vec![
            statement("UPDATE repository_sequences SET last = last + 1 WHERE kind = 'release' AND last < 9007199254740991", vec![]),
            statement("SELECT last FROM repository_sequences WHERE kind = 'release'", vec![]),
        ] })?;
        if sequence[0].rows_affected != 1 {
            return Err(crate::cells::Error::Invalid(
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
        Ok(MutationResult::Success(create_outcome(release)))
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

impl Mutation for UpdateRelease {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 25;
    type Input = UpdateReleaseInput;
    type Output = UpdateReleaseOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_number(input.number)?;
        validate_number(input.version)?;
        validate_title(&input.title)?;
        validate_body(&input.body, false)?;
        let Some(mut release) = load_release(context, input.number)? else {
            return Ok(MutationResult::Rejected(UpdateReleaseOutcome::NotFound));
        };
        if release.deleted {
            return Ok(MutationResult::Rejected(UpdateReleaseOutcome::NotFound));
        }
        if let Some(pending) = &release.publication_pending {
            let same = pending.expected_version == input.version
                && pending.title == input.title
                && pending.body == input.body
                && pending.prerelease == input.prerelease
                && !input.draft;
            return if same {
                Ok(MutationResult::Success(
                    UpdateReleaseOutcome::PublicationPending(Box::new(release)),
                ))
            } else {
                Ok(MutationResult::Rejected(UpdateReleaseOutcome::Conflict))
            };
        }
        if release.version != input.version {
            return Ok(MutationResult::Rejected(UpdateReleaseOutcome::Conflict));
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
            return Ok(MutationResult::Success(
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
        Ok(MutationResult::Success(UpdateReleaseOutcome::Updated(
            Box::new(release),
        )))
    }
}

pub(crate) struct CompleteReleasePublication;

impl Mutation for CompleteReleasePublication {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 26;
    type Input = CompleteReleasePublicationInput;
    type Output = CompleteReleasePublicationOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_number(input.number)?;
        if input.expected_version > 0 {
            validate_number(input.expected_version)?;
        }
        validate_oid(&input.tag_oid)?;
        let Some(mut release) = load_release(context, input.number)? else {
            return Ok(MutationResult::Rejected(
                CompleteReleasePublicationOutcome::NotFound,
            ));
        };
        if release.deleted {
            return Ok(MutationResult::Rejected(
                CompleteReleasePublicationOutcome::NotFound,
            ));
        }
        let Some(pending) = release.publication_pending.clone() else {
            return if release.tag_oid.as_deref() == Some(&input.tag_oid)
                && release.version == publication_result_version(input.expected_version)?
            {
                Ok(MutationResult::Success(
                    CompleteReleasePublicationOutcome::Completed(Box::new(release)),
                ))
            } else {
                Ok(MutationResult::Rejected(
                    CompleteReleasePublicationOutcome::Conflict,
                ))
            };
        };
        if pending.expected_version != input.expected_version {
            return Ok(MutationResult::Rejected(
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
        Ok(MutationResult::Success(
            CompleteReleasePublicationOutcome::Completed(Box::new(release)),
        ))
    }
}

pub(crate) struct DeleteRelease;

impl Mutation for DeleteRelease {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 27;
    type Input = DeleteReleaseInput;
    type Output = DeleteReleaseOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_number(input.number)?;
        validate_number(input.version)?;
        let Some(mut release) = load_release(context, input.number)? else {
            return Ok(MutationResult::Rejected(DeleteReleaseOutcome::NotFound));
        };
        if release.deleted {
            return if input.version.checked_add(1) == Some(release.version) {
                Ok(MutationResult::Success(DeleteReleaseOutcome::Deleted))
            } else {
                Ok(MutationResult::Rejected(DeleteReleaseOutcome::NotFound))
            };
        }
        if release.publication_pending.is_some() {
            return Ok(MutationResult::Rejected(
                DeleteReleaseOutcome::PublicationPending,
            ));
        }
        if release.version != input.version {
            return Ok(MutationResult::Rejected(DeleteReleaseOutcome::Conflict));
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
        Ok(MutationResult::Success(DeleteReleaseOutcome::Deleted))
    }
}

pub(crate) struct GetRelease;

impl Read for GetRelease {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 24;
    type Input = u64;
    type Output = Option<ReleaseRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        number: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(number)?;
        load_release_query(context, number)
    }
}

pub(crate) struct GetReleaseByTag;

impl Read for GetReleaseByTag {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 37;
    type Input = String;
    type Output = Option<ReleaseRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        tag: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_tag_name(&tag)?;
        // Tag claims are unique and indexed. Resolve the record in this query's
        // snapshot instead of scanning release pages or opening a second Cell.
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT release_number FROM repository_release_tag_claims WHERE tag_name = ?",
                vec![SqlValue::Text(tag)],
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

pub(crate) struct GetReleaseSubmission;

impl Read for GetReleaseSubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 25;
    type Input = ReleaseSubmissionKey;
    type Output = Option<ReleaseRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
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

impl Read for ListReleases {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 26;
    type Input = ReleaseListInput;
    type Output = ReleasePage;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
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
                crate::cells::Error::Invalid("repository release list row disappeared"),
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
