use super::*;

const KEY: &str = "organization/projects/v1/default-access";

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DefaultAccess {
    pub version: u64,
    pub role: AccessRole,
}

pub(super) fn load(context: &Transaction<'_>) -> Result<DefaultAccess> {
    Ok(context.document(KEY)?.unwrap_or_default())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "OrganizationProjectPolicy"))]
pub(crate) struct Preview {
    pub version: u64,
    pub role: AccessRole,
    pub affected_projects: usize,
    pub scope: String,
}

fn preview(context: &Transaction<'_>, policy: DefaultAccess) -> Result<Preview> {
    let rows = context.query(
        "SELECT id FROM organization_projects WHERE json_extract(data,'$.access.base') IS NULL ORDER BY id",
        &[],
    )?;
    // Bind the confirmation to the inheriting set. Unrelated item edits
    // can advance a Project revision without changing this decision.
    let mut scope = blake3::Hasher::new();
    scope.update(b"crab/project-default-access/v1");
    for row in &rows.rows {
        scope.update(uuid(&row[0])?.as_bytes());
    }
    Ok(Preview {
        version: policy.version,
        role: policy.role,
        affected_projects: rows.rows.len(),
        scope: scope.finalize().to_hex().to_string(),
    })
}

pub(crate) struct GetDefaultAccess;
impl Read for GetDefaultAccess {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 92;
    type Input = Caller;
    type Output = Preview;

    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Preview> {
        preview(context, load(context)?)
    }
}

pub(super) fn change(
    context: &Transaction<'_>,
    actor: Uuid,
    version: u64,
    role: AccessRole,
    scope: &str,
) -> Result<Option<u64>> {
    let current = load(context)?;
    if current.version != version {
        return Ok(None);
    }
    let confirmed = preview(context, current)?;
    if confirmed.scope != scope {
        return Ok(None);
    }
    if role == current.role {
        return Ok(Some(version));
    }
    let version = version
        .checked_add(1)
        .filter(|version| *version <= MAX_VERSION)
        .ok_or(Error::Invalid("Project policy version limit reached"))?;
    context.put_document(KEY, &DefaultAccess { version, role })?;
    advance(
        context,
        actor,
        Action::ProjectPolicyChanged {
            before: current.role,
            after: role,
            affected_projects: confirmed.affected_projects,
        },
    )?;
    Ok(Some(version))
}
