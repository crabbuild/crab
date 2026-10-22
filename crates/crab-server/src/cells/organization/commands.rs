use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BootstrapInput {
    pub actor: Uuid,
    pub name: String,
    pub description: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditInput {
    pub actor: Uuid,
    pub version: u64,
    pub name: String,
    pub description: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberChange {
    pub actor: Uuid,
    pub member: Uuid,
    pub version: u64,
    pub role: Option<Role>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum ChangeOutcome<T> {
    Applied(T),
    Conflict,
    LastOwner,
    NotFound,
}

pub(crate) struct Bootstrap;
impl Mutation for Bootstrap {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 1;
    type Input = BootstrapInput;
    type Output = ChangeOutcome<Organization>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        actor(input.actor)?;
        details(&input.name, &input.description)?;
        let digest = blake3::hash(&serde_json::to_vec(&input)?);
        let old = context.query(
            "SELECT actor, digest FROM organization_bootstrap WHERE singleton = 1",
            &[],
        )?;
        if let Some(row) = old.rows.first() {
            let [owner, SqlValue::Blob(old_digest)] = row.as_slice() else {
                return Err(Error::Invalid("organization bootstrap record is invalid"));
            };
            if uuid(owner)? != input.actor || old_digest.as_slice() != digest.as_bytes() {
                return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
            }
            let value =
                record(context)?.ok_or(Error::Invalid("organization bootstrap has no record"))?;
            return Ok(MutationResult::Success(ChangeOutcome::Applied(value)));
        }
        context.query("INSERT INTO organization_details(singleton, name, description, created_at_ms, updated_at_ms) VALUES (1, ?, ?, ?, ?)", &[SqlValue::Text(input.name), SqlValue::Text(input.description), SqlValue::Integer(context.now_ms()), SqlValue::Integer(context.now_ms())])?;
        context.query("INSERT INTO organization_members(actor, role, active, version) VALUES (?, 'owner', 1, 1)", &[blob(input.actor)])?;
        context.query(
            "INSERT INTO organization_bootstrap(singleton, actor, digest) VALUES (1, ?, ?)",
            &[
                blob(input.actor),
                SqlValue::Blob(digest.as_bytes().to_vec()),
            ],
        )?;
        advance(context, input.actor, Action::Created)?;
        let value =
            record(context)?.ok_or(Error::Invalid("organization creation has no record"))?;
        Ok(MutationResult::Success(ChangeOutcome::Applied(value)))
    }
}
pub(crate) struct Edit;
impl Mutation for Edit {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 2;
    type Input = EditInput;
    type Output = ChangeOutcome<Organization>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        details(&input.name, &input.description)?;
        let Some(mut current) = record(context)? else {
            return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
        };
        if current.version != input.version {
            return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
        }
        if current.name != input.name || current.description != input.description {
            // Wall-clock rollback cannot make valid persisted metadata unreadable.
            let now = context.now_ms().max(current.updated_at_ms as i64);
            let action = Action::Edited {
                name: current.name != input.name,
                description: current.description != input.description,
            };
            context.query("UPDATE organization_details SET name = ?, description = ?, updated_at_ms = ? WHERE singleton = 1", &[SqlValue::Text(input.name), SqlValue::Text(input.description), SqlValue::Integer(now)])?;
            advance(context, input.actor, action)?;
            current =
                record(context)?.ok_or(Error::Invalid("organization edit lost its record"))?;
        }
        Ok(MutationResult::Success(ChangeOutcome::Applied(current)))
    }
}
pub(crate) struct ChangeMember;
impl Mutation for ChangeMember {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 3;
    type Input = MemberChange;
    type Output = ChangeOutcome<Member>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        actor(input.member)?;
        let Some(old) = member(context, input.member)?.filter(|member| member.active) else {
            return Ok(MutationResult::Rejected(ChangeOutcome::NotFound));
        };
        if old.version != input.version {
            return Ok(MutationResult::Rejected(ChangeOutcome::Conflict));
        }
        let role = input.role.unwrap_or(old.role);
        let active = input.role.is_some();
        if old.active == active && old.role == role {
            return Ok(MutationResult::Success(ChangeOutcome::Applied(old)));
        }
        if old.role == Role::Owner && (!active || role != Role::Owner) {
            let others = context.query("SELECT 1 FROM organization_members WHERE role = 'owner' AND active = 1 AND actor <> ? LIMIT 1", &[blob(input.member)])?;
            if others.rows.is_empty() {
                return Ok(MutationResult::Rejected(ChangeOutcome::LastOwner));
            }
        }
        let version = input
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_VERSION)
            .ok_or(Error::Invalid(
                "organization membership version limit reached",
            ))?;
        // Removal retains the version that invitation acceptance must advance.
        // An edit captured before removal cannot replace the later grant.
        context.query(
            "UPDATE organization_members SET role = ?, active = ?, version = ? WHERE actor = ?",
            &[
                SqlValue::Text(role.name().into()),
                SqlValue::Integer(i64::from(active)),
                integer(version)?,
                blob(input.member),
            ],
        )?;
        advance(
            context,
            input.actor,
            Action::Membership {
                member: input.member,
                role: input.role,
                version,
            },
        )?;
        Ok(MutationResult::Success(ChangeOutcome::Applied(Member {
            actor: input.member,
            role,
            active,
            version,
        })))
    }
}
