mod codec;
pub(super) mod merge_methods;
pub(crate) mod rulesets;

use super::*;

const MAX_PROTECTION_BYTES: u32 = 64 * 1024;

pub(super) fn archived(context: &Transaction<'_>) -> crate::cells::Result<bool> {
    // Read lifecycle in the mutation's transaction so a queued command cannot
    // write after archive commits. Runtime receipt replay precedes this check.
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT archived FROM repository_settings WHERE singleton = 1",
            vec![],
        )],
    })?;
    Ok(result_u64(&result, 0, 0)? != 0)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BranchProtectionRecord {
    pub branch: String,
    pub required_approvals: u8,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required_code_owner_reviews: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_last_push_approval: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_conversation_resolution: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_up_to_date: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_merge_queue: bool,
    pub require_linear_history: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_deletions: bool,
    pub required_checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_integrations: Vec<uuid::Uuid>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BranchProtectionSettings {
    pub version: u64,
    pub rules: Vec<BranchProtectionRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RepositoryLifecycleRecord {
    pub version: u64,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ReplaceBranchProtectionsInput {
    pub expected_version: u64,
    pub rules: Vec<BranchProtectionRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ReplaceBranchProtectionsOutcome {
    Updated(BranchProtectionSettings),
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ReplaceRepositoryLifecycleInput {
    pub expected_version: u64,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ReplaceRepositoryLifecycleOutcome {
    Updated(RepositoryLifecycleRecord),
    Conflict,
    Unchanged,
}

pub(crate) struct ReplaceBranchProtections;

impl Mutation for ReplaceBranchProtections {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 14;
    type Input = ReplaceBranchProtectionsInput;
    type Output = ReplaceBranchProtectionsOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_settings_version(input.expected_version, true)?;
        validate_protections(&input.rules)?;
        let current = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT protections_version FROM repository_settings WHERE singleton = 1",
                vec![],
            )],
        })?;
        let version = result_u64(&current, 0, 0)?;
        if version != input.expected_version || version >= MAX_NUMBER - 1 {
            return Ok(MutationResult::Rejected(
                ReplaceBranchProtectionsOutcome::Conflict,
            ));
        }
        let settings = BranchProtectionSettings {
            version: version + 1,
            rules: input.rules,
        };
        context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_settings SET protections_version = ?, protections = ? WHERE singleton = 1 AND protections_version = ?",
                vec![
                    integer(settings.version)?,
                    SqlValue::Blob(encode_protections(&settings.rules)?),
                    integer(version)?,
                ],
            )],
        })?;
        advance_revision(context)?;
        Ok(MutationResult::Success(
            ReplaceBranchProtectionsOutcome::Updated(settings),
        ))
    }
}

pub(crate) struct ReplaceRepositoryLifecycle;

impl Mutation for ReplaceRepositoryLifecycle {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 15;
    type Input = ReplaceRepositoryLifecycleInput;
    type Output = ReplaceRepositoryLifecycleOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        validate_settings_version(input.expected_version, true)?;
        let current = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT lifecycle_version, archived FROM repository_settings WHERE singleton = 1",
                vec![],
            )],
        })?;
        let version = result_u64(&current, 0, 0)?;
        let archived = result_u64(&current, 0, 1)? != 0;
        if version != input.expected_version || version >= MAX_NUMBER - 1 {
            return Ok(MutationResult::Rejected(
                ReplaceRepositoryLifecycleOutcome::Conflict,
            ));
        }
        if archived == input.archived {
            return Ok(MutationResult::Rejected(
                ReplaceRepositoryLifecycleOutcome::Unchanged,
            ));
        }
        let lifecycle = RepositoryLifecycleRecord {
            version: version + 1,
            archived: input.archived,
        };
        context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_settings SET lifecycle_version = ?, archived = ? WHERE singleton = 1 AND lifecycle_version = ?",
                vec![
                    integer(lifecycle.version)?,
                    SqlValue::Integer(i64::from(lifecycle.archived)),
                    integer(version)?,
                ],
            )],
        })?;
        advance_revision(context)?;
        Ok(MutationResult::Success(
            ReplaceRepositoryLifecycleOutcome::Updated(lifecycle),
        ))
    }
}

pub(crate) struct GetBranchProtections;

impl Read for GetBranchProtections {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 13;
    type Input = ();
    type Output = Option<BranchProtectionSettings>;

    fn execute(
        context: &mut Transaction<'_>,
        (): Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        load_branch_protections(context)
    }
}

pub(super) fn load_branch_protections(
    context: &Transaction<'_>,
) -> crate::cells::Result<Option<BranchProtectionSettings>> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT protections_version, protections FROM repository_settings WHERE singleton = 1",
            vec![],
        )],
    })?;
    let version = result_u64(&result, 0, 0)?;
    if version == 0 {
        return Ok(None);
    }
    let rules = decode_protections(result_blob(&result[0].rows[0], 1)?)?;
    let settings = BranchProtectionSettings { version, rules };
    validate_settings(&settings)?;
    Ok(Some(settings))
}

pub(super) fn requires_merge_queue(
    context: &Transaction<'_>,
    reference: &str,
    configured: bool,
) -> crate::cells::Result<bool> {
    let Some(branch) = reference.strip_prefix("refs/heads/") else {
        return Ok(false);
    };
    Ok(
        load_branch_protections(context)?.map_or(configured, |stored| {
            stored
                .rules
                .iter()
                .any(|rule| rule.branch == branch && rule.require_merge_queue)
        }),
    )
}

pub(crate) struct GetRepositoryLifecycle;

impl Read for GetRepositoryLifecycle {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 14;
    type Input = ();
    type Output = RepositoryLifecycleRecord;

    fn execute(
        context: &mut Transaction<'_>,
        (): Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT lifecycle_version, archived FROM repository_settings WHERE singleton = 1",
                vec![],
            )],
        })?;
        let lifecycle = RepositoryLifecycleRecord {
            version: result_u64(&result, 0, 0)?,
            archived: result_u64(&result, 0, 1)? != 0,
        };
        validate_settings_version(lifecycle.version, true)?;
        Ok(lifecycle)
    }
}

fn validate_settings(settings: &BranchProtectionSettings) -> crate::cells::Result<()> {
    validate_settings_version(settings.version, false)?;
    validate_protections(&settings.rules)
}

fn validate_settings_version(version: u64, allow_zero: bool) -> crate::cells::Result<()> {
    if version >= MAX_NUMBER || (!allow_zero && version == 0) {
        return Err(crate::cells::Error::Invalid(
            "repository settings version is invalid",
        ));
    }
    Ok(())
}

pub(in crate::cells::repository) fn validate_protections(
    rules: &[BranchProtectionRecord],
) -> crate::cells::Result<()> {
    let rules = rules
        .iter()
        .map(|rule| crate::BranchProtection {
            branch: rule.branch.clone(),
            required_approvals: rule.required_approvals,
            required_code_owner_reviews: rule.required_code_owner_reviews,
            require_last_push_approval: rule.require_last_push_approval,
            require_conversation_resolution: rule.require_conversation_resolution,
            require_up_to_date: rule.require_up_to_date,
            require_merge_queue: rule.require_merge_queue,
            require_linear_history: rule.require_linear_history,
            allow_deletions: rule.allow_deletions,
            required_checks: rule.required_checks.clone(),
            required_integrations: rule.required_integrations.clone(),
        })
        .collect::<Vec<_>>();
    if !crate::config::valid_branch_protections(&rules) {
        return Err(crate::cells::Error::Invalid(
            "repository branch protections are invalid",
        ));
    }
    Ok(())
}

fn encode_protections(rules: &[BranchProtectionRecord]) -> crate::cells::Result<Vec<u8>> {
    codec::encode(rules)
        .map_err(|_| crate::cells::Error::Invalid("repository protections are too large"))
}

fn decode_protections(bytes: &[u8]) -> crate::cells::Result<Vec<BranchProtectionRecord>> {
    let rules = codec::decode(bytes)
        .map_err(|_| crate::cells::Error::Invalid("repository protections are invalid"))?;
    validate_protections(&rules)?;
    Ok(rules)
}

#[cfg(test)]
mod tests;
