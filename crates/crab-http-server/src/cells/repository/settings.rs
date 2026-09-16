use super::*;

const MAX_PROTECTION_BYTES: u32 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchProtectionRecord {
    pub branch: String,
    pub required_approvals: u8,
    pub required_checks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchProtectionSettings {
    pub version: u64,
    pub rules: Vec<BranchProtectionRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepositoryLifecycleRecord {
    pub version: u64,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReplaceBranchProtectionsInput {
    pub expected_version: u64,
    pub rules: Vec<BranchProtectionRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplaceBranchProtectionsOutcome {
    Updated(BranchProtectionSettings),
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReplaceRepositoryLifecycleInput {
    pub expected_version: u64,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplaceRepositoryLifecycleOutcome {
    Updated(RepositoryLifecycleRecord),
    Conflict,
    Unchanged,
}

pub(crate) struct ReplaceBranchProtections;

impl Command for ReplaceBranchProtections {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 14;
    const CODEC_VERSION: u32 = 1;
    type Input = ReplaceBranchProtectionsInput;
    type Output = ReplaceBranchProtectionsOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
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
            return Ok(CommandResult::Rejected(
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
        Ok(CommandResult::Success(
            ReplaceBranchProtectionsOutcome::Updated(settings),
        ))
    }
}

pub(crate) struct ReplaceRepositoryLifecycle;

impl Command for ReplaceRepositoryLifecycle {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 15;
    const CODEC_VERSION: u32 = 1;
    type Input = ReplaceRepositoryLifecycleInput;
    type Output = ReplaceRepositoryLifecycleOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
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
            return Ok(CommandResult::Rejected(
                ReplaceRepositoryLifecycleOutcome::Conflict,
            ));
        }
        if archived == input.archived {
            return Ok(CommandResult::Rejected(
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
        Ok(CommandResult::Success(
            ReplaceRepositoryLifecycleOutcome::Updated(lifecycle),
        ))
    }
}

pub(crate) struct GetBranchProtections;

impl Query for GetBranchProtections {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 13;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = Option<BranchProtectionSettings>;

    fn execute(
        context: &mut QueryContext<'_>,
        (): Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
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
}

pub(crate) struct GetRepositoryLifecycle;

impl Query for GetRepositoryLifecycle {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 14;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = RepositoryLifecycleRecord;

    fn execute(
        context: &mut QueryContext<'_>,
        (): Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
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

fn validate_settings(settings: &BranchProtectionSettings) -> crab_cell_runtime::Result<()> {
    validate_settings_version(settings.version, false)?;
    validate_protections(&settings.rules)
}

fn validate_settings_version(version: u64, allow_zero: bool) -> crab_cell_runtime::Result<()> {
    if version >= MAX_NUMBER || (!allow_zero && version == 0) {
        return Err(crab_cell_runtime::Error::Command(
            "repository settings version is invalid",
        ));
    }
    Ok(())
}

fn validate_protections(rules: &[BranchProtectionRecord]) -> crab_cell_runtime::Result<()> {
    let rules = rules
        .iter()
        .map(|rule| crate::BranchProtection {
            branch: rule.branch.clone(),
            required_approvals: rule.required_approvals,
            required_checks: rule.required_checks.clone(),
        })
        .collect::<Vec<_>>();
    if !crate::config::valid_branch_protections(&rules) {
        return Err(crab_cell_runtime::Error::Command(
            "repository branch protections are invalid",
        ));
    }
    Ok(())
}

fn encode_protections(rules: &[BranchProtectionRecord]) -> crab_cell_runtime::Result<Vec<u8>> {
    let mut encoder = BoundedEncoder::new(MAX_PROTECTION_BYTES)
        .map_err(|_| crab_cell_runtime::Error::Command("repository protections are too large"))?;
    encoder
        .write_count(rules.len())
        .map_err(|_| crab_cell_runtime::Error::Command("repository protections are too large"))?;
    for rule in rules {
        rule.encode(&mut encoder).map_err(|_| {
            crab_cell_runtime::Error::Command("repository protections are too large")
        })?;
    }
    Ok(encoder.finish())
}

fn decode_protections(bytes: &[u8]) -> crab_cell_runtime::Result<Vec<BranchProtectionRecord>> {
    let mut decoder = BoundedDecoder::new(bytes, MAX_PROTECTION_BYTES)
        .map_err(|_| crab_cell_runtime::Error::Command("repository protections are invalid"))?;
    let count = decoder
        .read_count()
        .map_err(|_| crab_cell_runtime::Error::Command("repository protections are invalid"))?;
    if count > 100 {
        return Err(crab_cell_runtime::Error::Command(
            "repository protections are invalid",
        ));
    }
    let mut rules = Vec::with_capacity(count);
    for _ in 0..count {
        rules.push(BranchProtectionRecord::decode(&mut decoder).map_err(|_| {
            crab_cell_runtime::Error::Command("repository protections are invalid")
        })?);
    }
    decoder
        .finish()
        .map_err(|_| crab_cell_runtime::Error::Command("repository protections are invalid"))?;
    validate_protections(&rules)?;
    Ok(rules)
}
